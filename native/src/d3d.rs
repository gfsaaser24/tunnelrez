//! DXGI adapter / output enumeration and D3D11 device helpers.

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory1, IDXGIOutput,
    DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::Graphics::Gdi::HMONITOR;

/// PCI vendor ID of NVIDIA.
pub const VENDOR_NVIDIA: u32 = 0x10de;
/// PCI vendor ID of Intel.
pub const VENDOR_INTEL: u32 = 0x8086;
/// PCI vendor ID of AMD.
pub const VENDOR_AMD: u32 = 0x1002;

/// Short vendor name for a PCI vendor ID ("other" if unknown).
pub fn vendor_name(id: u32) -> &'static str {
    match id {
        VENDOR_NVIDIA => "NVIDIA",
        VENDOR_INTEL => "Intel",
        VENDOR_AMD => "AMD",
        0x1414 => "Microsoft",
        _ => "other",
    }
}

/// A NUL-terminated (or full-length) UTF-16 buffer as a `String`; invalid UTF-16 is replaced.
pub fn wstr(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

/// An adapter LUID as one 64-bit number (`HighPart` in the upper 32 bits), for logs and comparisons.
pub fn luid_u64(l: LUID) -> u64 {
    (u64::from(l.HighPart.cast_unsigned()) << 32) | u64::from(l.LowPart)
}

/// One DXGI adapter (GPU) of this process's DXGI factory.
#[derive(Clone)]
pub struct Adapter {
    /// Index for `IDXGIFactory1::EnumAdapters1`.
    pub index: u32,
    /// Adapter description from the driver.
    pub name: String,
    /// PCI vendor ID.
    pub vendor_id: u32,
    /// Locally unique ID; the same GPU has the same LUID in DXGI, D3D12 and NVENC.
    pub luid: LUID,
    /// A software adapter (WARP / Basic Render Driver).
    pub software: bool,
    /// The DXGI interface.
    pub adapter: IDXGIAdapter1,
}

impl std::fmt::Debug for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {} ({}, luid {:#x})", self.index, self.name, vendor_name(self.vendor_id), luid_u64(self.luid))
    }
}

/// All adapters, in DXGI enumeration order.
///
/// # Errors
///
/// The DXGI factory could not be created, or an adapter description could not be read.
pub fn adapters() -> Result<Vec<Adapter>> {
    let mut v = Vec::new();
    // SAFETY: CreateDXGIFactory1 has no preconditions; it returns an owned factory or an error.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut i = 0;
    // SAFETY: EnumAdapters1 on the live factory returns an owned adapter, or an error past the end.
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        // SAFETY: GetDesc1 on the live adapter returns the descriptor by value.
        let d = unsafe { a.GetDesc1() }?;
        v.push(Adapter {
            index: i,
            name: wstr(&d.Description),
            vendor_id: d.VendorId,
            luid: d.AdapterLuid,
            software: (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0.cast_unsigned()) != 0,
            adapter: a,
        });
        i += 1;
    }
    Ok(v)
}

/// First hardware adapter of a vendor.
///
/// # Errors
///
/// Adapter enumeration failed, or no hardware adapter of that vendor exists.
pub fn adapter_by_vendor(vendor: u32) -> Result<Adapter> {
    adapters()?
        .into_iter()
        .find(|a| a.vendor_id == vendor && !a.software)
        .ok_or_else(|| anyhow!("no {} adapter", vendor_name(vendor)))
}

/// The adapter with this LUID.
///
/// # Errors
///
/// Adapter enumeration failed, or no adapter has that LUID.
pub fn adapter_by_luid(luid: LUID) -> Result<Adapter> {
    adapters()?
        .into_iter()
        .find(|a| a.luid.LowPart == luid.LowPart && a.luid.HighPart == luid.HighPart)
        .ok_or_else(|| anyhow!("no adapter with luid {:#x}", luid_u64(luid)))
}

/// One desktop output as this process's DXGI sees it.
#[derive(Clone, Debug)]
pub struct Output {
    /// The adapter that owns the output in this process's DXGI view.
    pub adapter: Adapter,
    /// Index for `IDXGIAdapter::EnumOutputs` on that adapter.
    pub output_idx: u32,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub device_name: String,
    /// Left edge in desktop coordinates.
    pub x: i32,
    /// Top edge in desktop coordinates.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// The monitor handle of the output.
    pub monitor: HMONITOR,
    /// The DXGI interface.
    pub output: IDXGIOutput,
}

/// All outputs of all adapters, in adapter then output order.
///
/// # Errors
///
/// Adapter enumeration failed, or an output description could not be read.
pub fn outputs() -> Result<Vec<Output>> {
    let mut v = Vec::new();
    for a in adapters()? {
        let mut o = 0;
        // SAFETY: EnumOutputs on a live adapter returns an owned output, or an error past the end.
        while let Ok(out) = unsafe { a.adapter.EnumOutputs(o) } {
            // SAFETY: GetDesc on the live output returns the descriptor by value.
            let d = unsafe { out.GetDesc() }?;
            let r = d.DesktopCoordinates;
            v.push(Output {
                adapter: a.clone(),
                output_idx: o,
                device_name: wstr(&d.DeviceName),
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                monitor: d.Monitor,
                output: out,
            });
            o += 1;
        }
    }
    Ok(v)
}

/// A D3D11 device with its immediate context, and the adapter it runs on.
#[derive(Clone, Debug)]
pub struct Device {
    /// The device (free-threaded).
    pub device: ID3D11Device,
    /// The immediate context, multithread-protected (see [`create_device`]).
    pub ctx: ID3D11DeviceContext,
    /// The adapter the device was created on.
    pub adapter: Adapter,
}

/// Create a D3D11 device (feature level 11.1 or 11.0, BGRA and video support) on `a`, with a
/// multithread-protected immediate context, frame latency 1 and the highest GPU thread priority.
///
/// # Errors
///
/// `D3D11CreateDevice` failed or returned no device or context.
pub fn create_device(a: &Adapter) -> Result<Device> {
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    // SAFETY: the adapter is live; `levels` and both out-params are locals that outlive the call.
    unsafe {
        D3D11CreateDevice(
            &a.adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&raw mut dev),
            None,
            Some(&raw mut ctx),
        )
    }
    .with_context(|| format!("D3D11CreateDevice on {a:?}"))?;
    let device = dev.ok_or_else(|| anyhow!("no device"))?;
    let ctx = ctx.ok_or_else(|| anyhow!("no context"))?;
    // Capture and encode run on their own threads but share the immediate context.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: COM call on a live interface; it only switches on the context's internal lock.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    // Keep the GPU queue short: we want each frame out as soon as possible.
    if let Ok(dx) = device.cast::<IDXGIDevice1>() {
        // SAFETY: COM call on a live interface with a value argument.
        let _ = unsafe { dx.SetMaximumFrameLatency(1) };
        // Our colour conversion and NVENC input must not queue behind other apps' rendering
        // (e.g. a 4K browser page at 98 % GPU). 7 = highest; best effort.
        let prio = if std::env::var_os("TUNNELREZ_NO_GPU_PRIORITY").is_some() { 0 } else { 7 };
        // SAFETY: COM call on a live interface with a value in the documented -7..=7 range.
        if let Err(e) = unsafe { dx.SetGPUThreadPriority(prio) } {
            crate::warn(format_args!("SetGPUThreadPriority({prio}) on {}: {e}", a.name));
        }
    }
    Ok(Device { device, ctx, adapter: a.clone() })
}

/// The descriptor of a texture.
pub fn tex_desc(t: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: GetDesc on a live texture writes into the local descriptor.
    unsafe { t.GetDesc(&raw mut d) };
    d
}

/// The adapter a D3D11 device lives on.
///
/// # Errors
///
/// The device is not a DXGI device, or its adapter could not be read.
pub fn device_luid(dev: &ID3D11Device) -> Result<LUID> {
    let dx: IDXGIDevice = dev.cast()?;
    // SAFETY: GetAdapter on a live DXGI device returns an owned adapter.
    let a = unsafe { dx.GetAdapter() }?;
    // SAFETY: GetDesc on the live adapter returns the descriptor by value.
    Ok(unsafe { a.GetDesc() }?.AdapterLuid)
}
