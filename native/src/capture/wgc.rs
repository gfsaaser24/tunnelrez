//! Windows.Graphics.Capture (WGC) monitor capture into a D3D11 device of our choice.
//!
//! WGC works from any process and any adapter: when the device is on another GPU than the one that renders
//! the monitor (hybrid laptop: Parsec virtual display rendered by the Intel iGPU, NVENC on the NVIDIA dGPU),
//! the OS copies each frame across adapters for us. It also composites the cursor and gives a QPC-based
//! capture timestamp.

use crate::clock;
use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use windows::core::{IInspectable, Interface};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// One captured frame; dropping it returns the buffer to the frame pool.
#[derive(Debug)]
pub struct WgcFrame {
    frame: Direct3D11CaptureFrame,
    /// The captured image (a pool buffer; valid until this frame is dropped).
    pub tex: ID3D11Texture2D,
    /// Capture time on the sender clock (`clock::now_us`).
    pub t_us: u64,
    /// Content width in pixels.
    pub width: u32,
    /// Content height in pixels.
    pub height: u32,
}

/// A `WinRT` size component as `u32`. Capture sizes are never negative; a negative value maps to 0.
fn dim(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

impl Drop for WgcFrame {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

/// An owned auto-reset Win32 event, signalled by the WGC callbacks.
#[derive(Debug)]
struct Event(HANDLE);
// SAFETY: Event only wraps a Win32 event handle; SetEvent and WaitForSingleObject on it are thread-safe.
unsafe impl Send for Event {}
// SAFETY: &Event only allows SetEvent / wait on the handle, which Win32 makes safe from many threads at once; the handle is closed only in Drop, when no reference is left.
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle was created by CreateEventW and is closed once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Windows.Graphics.Capture of one monitor into a free-threaded frame pool.
#[derive(Debug)]
pub struct WgcCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    closed_token: i64,
    device: IDirect3DDevice,
    size: SizeInt32,
    event: Arc<Event>,
    token: i64,
    /// Frames the pool has signalled (for diagnostics).
    pub arrived: Arc<AtomicU64>,
    /// The capture item closed (display removed): no more frames will come.
    closed: Arc<AtomicBool>,
}

// WinRT objects created free-threaded; the capture is used from one thread at a time.
// SAFETY: WgcCapture owns free-threaded (agile) WinRT objects and a D3D device; it lives inside one NativeSession used on one thread. The FrameArrived and Closed callbacks only touch the shared Event and atomics.
unsafe impl Send for WgcCapture {}

/// Initialise `WinRT` (multithreaded apartment) on the calling thread; repeated calls are harmless.
pub fn init_winrt() {
    // SAFETY: RoInitialize has no pointer arguments; a second call on the thread just returns S_FALSE.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
}

impl WgcCapture {
    /// Start capturing `monitor` into textures of `dev` (cursor included, no border).
    ///
    /// # Errors
    ///
    /// WGC is unavailable, the monitor cannot be captured, or a `WinRT` / D3D object could not be created.
    pub fn new(dev: &ID3D11Device, monitor: HMONITOR) -> Result<Self> {
        init_winrt();
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: `monitor` is a monitor handle from DXGI; an invalid one only makes the call fail.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")?;
        let dxgi: IDXGIDevice = dev.cast()?;
        // SAFETY: `dxgi` is a live DXGI device; the WinRT wrapper takes its own reference.
        let insp: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?;
        let device: IDirect3DDevice = insp.cast()?;
        let size = item.Size()?;
        // 2 buffers: one being written by DWM, one held by us while we copy it out.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Optional knobs: older Windows builds lack some of them.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Deliver every composed frame (default min interval may throttle).
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 }); // 1 ms
                                                                             // SAFETY: an unnamed auto-reset event with default security; `Event` owns and closes the handle.
        let ev = unsafe { CreateEventW(None, false, false, None) }?;
        let event = Arc::new(Event(ev));
        let arrived = Arc::new(AtomicU64::new(0));
        let token = {
            let (event, arrived) = (Arc::clone(&event), Arc::clone(&arrived));
            pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
                arrived.fetch_add(1, Ordering::Relaxed);
                // SAFETY: the closure holds an `Arc<Event>`, so the handle stays open while it can run.
                let _ = unsafe { SetEvent(event.0) };
                Ok(())
            }))?
        };
        let closed = Arc::new(AtomicBool::new(false));
        let closed_token = {
            let (closed, event) = (Arc::clone(&closed), Arc::clone(&event));
            item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                closed.store(true, Ordering::SeqCst);
                // SAFETY: the closure holds an `Arc<Event>`, so the handle stays open while it can run.
                let _ = unsafe { SetEvent(event.0) };
                Ok(())
            }))?
        };
        session.StartCapture()?;
        Ok(WgcCapture { pool, session, item, closed_token, device, size, event, token, arrived, closed })
    }

    /// The monitor went away (WGC cannot recover; start a new capture).
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Current capture size (width, height) in pixels.
    pub fn size(&self) -> (u32, u32) {
        (dim(self.size.Width), dim(self.size.Height))
    }

    /// Newest frame, waiting up to `timeout_ms` for one. Older queued frames are discarded.
    ///
    /// # Errors
    ///
    /// A `WinRT` call on the frame failed, or the frame pool could not be re-created after a size change.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Option<WgcFrame>> {
        let mut f = self.pool.TryGetNextFrame().ok();
        if f.is_none() {
            // SAFETY: the event handle is owned by `self.event` and stays open during the wait.
            let w = unsafe { WaitForSingleObject(self.event.0, timeout_ms) };
            if w != WAIT_OBJECT_0 {
                return Ok(None);
            }
            f = self.pool.TryGetNextFrame().ok();
        }
        let Some(mut frame) = f else { return Ok(None) };
        // Drain: keep only the newest.
        while let Ok(n) = self.pool.TryGetNextFrame() {
            let _ = frame.Close();
            frame = n;
        }
        let cs = frame.ContentSize()?;
        if cs.Width != self.size.Width || cs.Height != self.size.Height {
            // Monitor mode changed: resize the pool; next frame has the new size.
            self.size = cs;
            let _ = frame.Close();
            self.pool.Recreate(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, cs)?;
            return Ok(None);
        }
        let t_us = clock::hns_to_us(frame.SystemRelativeTime()?.Duration);
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: GetInterface on the frame surface; the frame stays alive (in WgcFrame) while the texture is used.
        let tex: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|e| anyhow!("surface texture: {e}"))?;
        Ok(Some(WgcFrame { frame, tex, t_us, width: dim(cs.Width), height: dim(cs.Height) }))
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
