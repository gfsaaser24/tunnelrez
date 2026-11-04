//! Moving an NV12 frame from the capture GPU (iGPU) to the encode GPU (dGPU).
//!
//! * `Cross`: GPU-to-GPU through a D3D12 cross-adapter buffer in system memory (the dGPU does not support
//!   cross-adapter row-major textures, so a buffer with copyable footprints carries both NV12 planes).
//!   D3D11 cannot share across adapters; each side bridges D3D11 <-> D3D12 with same-adapter NT handles and
//!   shared fences. No CPU copy; both copies run on copy queues (DMA engines).
//! * `Staging`: map the frame on the iGPU, memcpy into a dGPU staging texture, copy that to the encoder input.

use crate::conv::usize_from_u32;
use crate::d3d::{tex_desc, Device};
use anyhow::{anyhow, bail, Context, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D, D3D11_CPU_ACCESS_FLAG,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12CreateDevice, ID3D12CommandAllocator, ID3D12CommandList, ID3D12CommandQueue, ID3D12Device, ID3D12Fence,
    ID3D12GraphicsCommandList, ID3D12Heap, ID3D12Resource, D3D12_COMMAND_LIST_TYPE_COPY, D3D12_COMMAND_QUEUE_DESC,
    D3D12_COMMAND_QUEUE_FLAG_NONE, D3D12_COMMAND_QUEUE_PRIORITY_HIGH, D3D12_FENCE_FLAG_SHARED,
    D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER, D3D12_HEAP_DESC, D3D12_HEAP_FLAG_SHARED,
    D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT,
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
    D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER, D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_COPY_LOCATION,
    D3D12_TEXTURE_COPY_LOCATION_0, D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Which transfer method to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// D3D12 cross-adapter buffer, GPU to GPU.
    Cross,
    /// Map, CPU copy, upload.
    Staging,
}

/// A frame transfer from a texture on the capture GPU to a texture on the encode GPU.
#[derive(Debug)]
pub enum Xfer {
    /// See [`Cross`].
    Cross(Cross),
    /// See [`Staging`].
    Staging(Staging),
}

impl Xfer {
    /// `src` lives on `cap`, `dst` on `enc`; same size and format (NV12).
    ///
    /// # Errors
    ///
    /// The textures are unsuitable (not shared for `Cross`, unsupported format for `Staging`), or a
    /// D3D11 / D3D12 object could not be created.
    pub fn new(kind: Kind, cap: &Device, src: &ID3D11Texture2D, enc: &Device, dst: &ID3D11Texture2D) -> Result<Xfer> {
        Ok(match kind {
            Kind::Cross => Xfer::Cross(Cross::new(cap, src, enc, dst)?),
            Kind::Staging => Xfer::Staging(Staging::new(cap, src, enc, dst)?),
        })
    }

    /// The transfer method in use.
    pub fn kind(&self) -> Kind {
        match self {
            Xfer::Cross(_) => Kind::Cross,
            Xfer::Staging(_) => Kind::Staging,
        }
    }

    /// Copy the current contents of `src` (after all GPU work queued on `cap` so far) into `dst`.
    /// Returns when `dst` is complete and safe for the encoder to read.
    ///
    /// # Errors
    ///
    /// A GPU call failed (e.g. device removed), a map failed, or the cross-adapter copy timed out (1 s).
    pub fn run(&mut self) -> Result<()> {
        match self {
            Xfer::Cross(x) => x.run(),
            Xfer::Staging(x) => x.run(),
        }
    }
}

fn shared_tex_handle(t: &ID3D11Texture2D) -> Result<HANDLE> {
    let r: IDXGIResource1 = t.cast()?;
    // SAFETY: CreateSharedHandle on a live shared texture; we own and close the returned handle.
    Ok(unsafe { r.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0, None) }?)
}

/// An owned Win32 handle, closed on drop.
#[derive(Debug)]
struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this wrapper and closed once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn d3d12_device(dev: &Device) -> Result<ID3D12Device> {
    let mut d: Option<ID3D12Device> = None;
    // SAFETY: D3D12CreateDevice gets a live adapter and a local out-param.
    unsafe { D3D12CreateDevice(&dev.adapter.adapter, D3D_FEATURE_LEVEL_11_0, &raw mut d) }
        .context("D3D12CreateDevice")?;
    d.ok_or_else(|| anyhow!("no D3D12 device"))
}

fn copy_queue(d: &ID3D12Device) -> Result<(ID3D12CommandQueue, ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
    let qd = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_COPY,
        Priority: D3D12_COMMAND_QUEUE_PRIORITY_HIGH.0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    // SAFETY: queue creation on a live device; the descriptor is a local that outlives the call.
    let q: ID3D12CommandQueue = unsafe { d.CreateCommandQueue(&raw const qd) }?;
    // SAFETY: allocator creation on a live device with a value argument.
    let a: ID3D12CommandAllocator = unsafe { d.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY) }?;
    // SAFETY: list creation on a live device with the live copy allocator just created on it.
    let l: ID3D12GraphicsCommandList = unsafe { d.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &a, None) }?;
    Ok((q, a, l))
}

fn loc_sub(r: &ID3D12Resource, sub: u32) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(r.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub },
    }
}

fn loc_buf(r: &ID3D12Resource, fp: D3D12_PLACED_SUBRESOURCE_FOOTPRINT) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(r.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp },
    }
}

fn drop_loc(mut l: D3D12_TEXTURE_COPY_LOCATION) {
    // SAFETY: every location built by loc_buf / loc_sub holds one AddRef'd resource, released exactly once, here.
    unsafe { ManuallyDrop::drop(&mut l.pResource) };
}

/// D3D11 side of a same-adapter D3D11 <-> D3D12 bridge: a D3D12 fence opened as an `ID3D11Fence`.
#[derive(Debug)]
struct Bridge {
    d12: ID3D12Device,
    q: ID3D12CommandQueue,
    _alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    /// D3D11 <-> D3D12 fence on this adapter.
    f12: ID3D12Fence,
    f11: ID3D11Fence,
    ctx4: ID3D11DeviceContext4,
}

impl Bridge {
    fn new(dev: &Device) -> Result<Self> {
        let d12 = d3d12_device(dev)?;
        let (q, alloc, list) = copy_queue(&d12)?;
        // SAFETY: fence creation on a live device with value arguments.
        let f12: ID3D12Fence = unsafe { d12.CreateFence(0, D3D12_FENCE_FLAG_SHARED) }?;
        // SAFETY: `f12` is a live shared fence of `d12`; the returned NT handle is owned (and closed) by `Owned`.
        let h = Owned(unsafe { d12.CreateSharedHandle(&f12, None, GENERIC_ALL.0, None) }?);
        let d5: ID3D11Device5 = dev.device.cast()?;
        let mut f11: Option<ID3D11Fence> = None;
        // SAFETY: `h` is a live fence handle of the same adapter (closed after this call); the out-param is a local.
        unsafe { d5.OpenSharedFence(h.0, &raw mut f11) }.context("open D3D12 fence in D3D11")?;
        let f11 = f11.context("OpenSharedFence returned no fence")?;
        let ctx4: ID3D11DeviceContext4 = dev.ctx.cast()?;
        Ok(Bridge { d12, q, _alloc: alloc, list, f12, f11, ctx4 })
    }

    fn open_tex(&self, t: &ID3D11Texture2D) -> Result<ID3D12Resource> {
        let h = Owned(shared_tex_handle(t)?);
        let mut r: Option<ID3D12Resource> = None;
        // SAFETY: the handle is a live NT handle for a texture of the same adapter; it is closed by Owned after opening.
        unsafe { self.d12.OpenSharedHandle(h.0, &raw mut r) }.context("open D3D11 texture in D3D12")?;
        r.ok_or_else(|| anyhow!("no resource"))
    }
}

/// GPU-to-GPU transfer through a D3D12 cross-adapter buffer (see the module docs).
#[derive(Debug)]
pub struct Cross {
    cap: Bridge,
    enc: Bridge,
    /// Cross-adapter fence: created on the capture adapter, opened on the encode adapter.
    xf_cap: ID3D12Fence,
    xf_enc: ID3D12Fence,
    _buf_cap: ID3D12Resource,
    _buf_enc: ID3D12Resource,
    v: u64,
    event: Owned,
    /// Size of the cross-adapter buffer (both NV12 planes with D3D12 footprint padding).
    pub buffer_bytes: u64,
}

// SAFETY: Cross exclusively owns its D3D12 devices, queues, fences, buffers and event; it lives inside one NativeSession used on one thread.
unsafe impl Send for Cross {}

/// Copyable footprints (up to two planes) and their total size for a resource on `dev`.
fn footprints(
    dev: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    planes: u32,
) -> ([D3D12_PLACED_SUBRESOURCE_FOOTPRINT; 2], u64) {
    let mut fps = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 2];
    let mut total = 0u64;
    // SAFETY: `planes` <= 2, so the call writes at most the two entries of `fps`; `desc` and `total` are live locals.
    unsafe { dev.GetCopyableFootprints(desc, 0, planes, 0, Some(fps.as_mut_ptr()), None, None, Some(&raw mut total)) };
    (fps, total)
}

impl Cross {
    fn new(cap: &Device, src: &ID3D11Texture2D, enc: &Device, dst: &ID3D11Texture2D) -> Result<Self> {
        for t in [src, dst] {
            let d = tex_desc(t);
            if d.MiscFlags & D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0.cast_unsigned() == 0 {
                bail!("cross-adapter transfer needs textures created with SHARED | SHARED_NTHANDLE");
            }
        }
        let cb = Bridge::new(cap)?;
        let eb = Bridge::new(enc)?;
        let src12 = cb.open_tex(src)?;
        let dst12 = eb.open_tex(dst)?;
        // SAFETY: GetDesc on a live resource returns the descriptor by value.
        let desc = unsafe { src12.GetDesc() };
        let planes: u32 = if desc.Format == DXGI_FORMAT_NV12 { 2 } else { 1 };
        let (fps, total) = footprints(&cb.d12, &desc, planes);
        // The same footprints must be valid on the encode adapter (both follow the D3D12 alignment rules).
        // SAFETY: GetDesc on a live resource returns the descriptor by value.
        let (_, total2) = footprints(&eb.d12, &unsafe { dst12.GetDesc() }, planes);
        if total != total2 {
            bail!("footprint mismatch between adapters ({total} vs {total2})");
        }

        let bd = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: total,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
        };
        // Cross-adapter buffers live in a shared heap (64 KB aligned); open the heap on the encode GPU.
        let size = (total + 65535) & !65535;
        let hd = D3D12_HEAP_DESC {
            SizeInBytes: size,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_SHARED | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER,
        };
        let mut heap_cap: Option<ID3D12Heap> = None;
        // SAFETY: heap creation on a live device; the descriptor and out-param are locals that outlive the call.
        unsafe { cb.d12.CreateHeap(&raw const hd, &raw mut heap_cap) }.context("cross-adapter heap")?;
        let heap_cap = heap_cap.context("CreateHeap returned no heap")?;
        // SAFETY: `heap_cap` is a live shared heap of `cb.d12`; the NT handle is owned (and closed) by `Owned`.
        let hh = Owned(unsafe { cb.d12.CreateSharedHandle(&heap_cap, None, GENERIC_ALL.0, None) }?);
        let mut heap_enc: Option<ID3D12Heap> = None;
        // SAFETY: `hh` is a live cross-adapter heap handle; the out-param is a local.
        unsafe { eb.d12.OpenSharedHandle(hh.0, &raw mut heap_enc) }.context("open cross-adapter heap on encode GPU")?;
        let heap_enc = heap_enc.context("OpenSharedHandle returned no heap")?;
        let mut buf: Option<ID3D12Resource> = None;
        // SAFETY: placed buffer at offset 0 of a live heap of at least `total` bytes; `bd` and `buf` are locals.
        unsafe {
            cb.d12.CreatePlacedResource(&heap_cap, 0, &raw const bd, D3D12_RESOURCE_STATE_COMMON, None, &raw mut buf)
        }
        .context("cross-adapter buffer")?;
        let mut b2: Option<ID3D12Resource> = None;
        // SAFETY: placed buffer at offset 0 of the encode device's view of the same live heap; `bd` and `b2` are locals.
        unsafe {
            eb.d12.CreatePlacedResource(&heap_enc, 0, &raw const bd, D3D12_RESOURCE_STATE_COMMON, None, &raw mut b2)
        }
        .context("buffer on encode GPU")?;
        let buf_enc = b2.context("CreatePlacedResource returned no buffer on the encode GPU")?;
        let buf_cap = buf.context("CreatePlacedResource returned no buffer")?;

        // SAFETY: fence creation on a live device with value arguments.
        let xf_cap: ID3D12Fence =
            unsafe { cb.d12.CreateFence(0, D3D12_FENCE_FLAG_SHARED | D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER) }?;
        // SAFETY: `xf_cap` is a live shared fence of `cb.d12`; the NT handle is owned (and closed) by `Owned`.
        let h = Owned(unsafe { cb.d12.CreateSharedHandle(&xf_cap, None, GENERIC_ALL.0, None) }?);
        let mut xf_enc: Option<ID3D12Fence> = None;
        // SAFETY: `h` is a live cross-adapter fence handle; the out-param is a local.
        unsafe { eb.d12.OpenSharedHandle(h.0, &raw mut xf_enc) }.context("open cross-adapter fence")?;
        let xf_enc = xf_enc.context("OpenSharedHandle returned no fence")?;

        // Record both copies once; they are replayed every frame.
        for (p, fp) in (0u32..).zip(fps.iter().take(usize_from_u32(planes))) {
            let (d, s) = (loc_buf(&buf_cap, *fp), loc_sub(&src12, p));
            // SAFETY: both locations hold live resources and a footprint/subresource that GetCopyableFootprints
            // produced for them; the list is open (not yet closed).
            unsafe { cb.list.CopyTextureRegion(&raw const d, 0, 0, 0, &raw const s, None) };
            drop_loc(d);
            drop_loc(s);
            let (d, s) = (loc_sub(&dst12, p), loc_buf(&buf_enc, *fp));
            // SAFETY: both locations hold live resources of the encode device and the same footprint/subresource
            // as the capture side; the list is open (not yet closed).
            unsafe { eb.list.CopyTextureRegion(&raw const d, 0, 0, 0, &raw const s, None) };
            drop_loc(d);
            drop_loc(s);
        }
        // SAFETY: closing a live, open command list.
        unsafe { cb.list.Close() }?;
        // SAFETY: closing a live, open command list.
        unsafe { eb.list.Close() }?;
        // SAFETY: an unnamed auto-reset event with default security; the handle is owned (and closed) by `Owned`.
        let event = Owned(unsafe { CreateEventW(None, false, false, None) }?);
        Ok(Cross {
            cap: cb,
            enc: eb,
            xf_cap,
            xf_enc,
            _buf_cap: buf_cap,
            _buf_enc: buf_enc,
            v: 0,
            event,
            buffer_bytes: total,
        })
    }

    fn run(&mut self) -> Result<()> {
        self.v += 1;
        let v = self.v;
        // iGPU: D3D11 work done -> D3D12 copy texture -> system-memory buffer -> cross-adapter fence.
        // SAFETY: context and fence are owned by `self.cap` and alive; `v` is above every earlier value.
        unsafe { self.cap.ctx4.Signal(&self.cap.f11, v) }?;
        // SAFETY: flushing a live context owned by `self.cap`; used only on this thread.
        unsafe { self.cap.ctx4.Flush() };
        // SAFETY: queue and fence are owned by `self.cap` and alive.
        unsafe { self.cap.q.Wait(&self.cap.f12, v) }?;
        let cap_list: ID3D12CommandList = self.cap.list.cast()?;
        // SAFETY: the list belongs to this queue's device and was closed after recording in `new`.
        unsafe { self.cap.q.ExecuteCommandLists(&[Some(cap_list)]) };
        // SAFETY: queue and cross-adapter fence are owned by `self` and alive; `v` only grows.
        unsafe { self.cap.q.Signal(&self.xf_cap, v) }?;
        // dGPU: wait for the iGPU copy -> D3D12 copy buffer -> encoder input texture.
        // SAFETY: queue and the opened cross-adapter fence are owned by `self` and alive.
        unsafe { self.enc.q.Wait(&self.xf_enc, v) }?;
        let enc_list: ID3D12CommandList = self.enc.list.cast()?;
        // SAFETY: the list belongs to this queue's device and was closed after recording in `new`.
        unsafe { self.enc.q.ExecuteCommandLists(&[Some(enc_list)]) };
        // SAFETY: queue and fence are owned by `self.enc` and alive; `v` only grows.
        unsafe { self.enc.q.Signal(&self.enc.f12, v) }?;
        // The encoder reads the texture through NVENC; wait on the CPU so ordering is guaranteed.
        // SAFETY: live fence owned by `self.enc`; the event handle stays open while `self.event` lives.
        unsafe { self.enc.f12.SetEventOnCompletion(v, self.event.0) }?;
        // SAFETY: waiting on the live event handle owned by `self.event`.
        if unsafe { WaitForSingleObject(self.event.0, 1000) } != WAIT_OBJECT_0 {
            bail!("cross-adapter copy timed out");
        }
        Ok(())
    }
}

/// Transfer through mapped staging textures and a CPU copy (see the module docs).
#[derive(Debug)]
pub struct Staging {
    cap_ctx: ID3D11DeviceContext,
    enc_ctx: ID3D11DeviceContext,
    src: ID3D11Texture2D,
    dst: ID3D11Texture2D,
    rd: ID3D11Texture2D,
    wr: ID3D11Texture2D,
    rows: u32,
    row_bytes: usize,
}

// SAFETY: Staging exclusively owns its textures and context references; it lives inside one NativeSession used on one thread, so the copy + map sequences never interleave with other context use.
unsafe impl Send for Staging {}

impl Staging {
    fn new(cap: &Device, src: &ID3D11Texture2D, enc: &Device, dst: &ID3D11Texture2D) -> Result<Self> {
        let d = tex_desc(src);
        let mk = |dev: &Device, cpu: D3D11_CPU_ACCESS_FLAG| -> Result<ID3D11Texture2D> {
            let sd = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: cpu.0.cast_unsigned(),
                MiscFlags: 0,
                ..d
            };
            let mut t = None;
            // SAFETY: CreateTexture2D reads the local descriptor and writes the local out-param.
            unsafe { dev.device.CreateTexture2D(&raw const sd, None, Some(&raw mut t)) }.context("staging texture")?;
            t.context("CreateTexture2D returned no staging texture")
        };
        let (rows, row_bytes) = match d.Format {
            DXGI_FORMAT_NV12 => (d.Height + d.Height / 2, usize_from_u32(d.Width)),
            DXGI_FORMAT_B8G8R8A8_UNORM => (d.Height, usize_from_u32(d.Width) * 4),
            f => bail!("staging: unsupported format {f:?}"),
        };
        Ok(Staging {
            cap_ctx: cap.ctx.clone(),
            enc_ctx: enc.ctx.clone(),
            src: src.clone(),
            dst: dst.clone(),
            rd: mk(cap, D3D11_CPU_ACCESS_READ)?,
            wr: mk(enc, D3D11_CPU_ACCESS_WRITE)?,
            rows,
            row_bytes,
        })
    }

    fn run(&mut self) -> Result<()> {
        // Both contexts are used only on this thread (see the Send impl), so no other work interleaves
        // between the copy and the maps.
        // SAFETY: `rd` and `src` are live textures of the capture device with the same size and format.
        unsafe { self.cap_ctx.CopyResource(&self.rd, &self.src) };
        let mut mr = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `rd` is a CPU-readable staging texture of this context's device; `mr` is a local out-param.
        unsafe { self.cap_ctx.Map(&self.rd, 0, D3D11_MAP_READ, 0, Some(&raw mut mr)) }.context("map iGPU staging")?;
        let mut mw = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `wr` is a CPU-writable staging texture of this context's device; `mw` is a local out-param.
        if let Err(e) = unsafe { self.enc_ctx.Map(&self.wr, 0, D3D11_MAP_WRITE, 0, Some(&raw mut mw)) } {
            // SAFETY: `rd` subresource 0 was mapped just above.
            unsafe { self.cap_ctx.Unmap(&self.rd, 0) };
            return Err(e).context("map dGPU staging");
        }
        let (sp, dp) = (usize_from_u32(mr.RowPitch), usize_from_u32(mw.RowPitch));
        let (s, d) = (mr.pData.cast::<u8>().cast_const(), mw.pData.cast::<u8>());
        // Both textures have `rows` rows of at least `row_bytes` bytes each, `sp` / `dp` apart; the
        // mappings stay valid until the Unmap calls below.
        if sp == dp {
            // SAFETY: equal pitches, so one copy covers all rows: the last row ends at `sp * (rows - 1) + row_bytes` in both mappings.
            unsafe { std::ptr::copy_nonoverlapping(s, d, sp * (usize_from_u32(self.rows) - 1) + self.row_bytes) };
        } else {
            for r in 0..usize_from_u32(self.rows) {
                // SAFETY: row `r` < `rows` starts at offset `r * sp`, inside the read mapping.
                let src_row = unsafe { s.add(r * sp) };
                // SAFETY: row `r` < `rows` starts at offset `r * dp`, inside the write mapping.
                let dst_row = unsafe { d.add(r * dp) };
                // SAFETY: each row holds at least `row_bytes` bytes in both mappings, and the two mappings
                // are separate allocations, so the ranges do not overlap.
                unsafe { std::ptr::copy_nonoverlapping(src_row, dst_row, self.row_bytes) };
            }
        }
        // SAFETY: `wr` subresource 0 is mapped; the pointers above are not used after this.
        unsafe { self.enc_ctx.Unmap(&self.wr, 0) };
        // SAFETY: `rd` subresource 0 is mapped; the pointers above are not used after this.
        unsafe { self.cap_ctx.Unmap(&self.rd, 0) };
        // SAFETY: `dst` and `wr` are live textures of the encode device with the same size and format.
        unsafe { self.enc_ctx.CopyResource(&self.dst, &self.wr) };
        Ok(())
    }
}
