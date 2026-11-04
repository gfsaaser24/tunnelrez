//! `TunnelRez` native backend (Windows): capture one desktop output and encode it with NVENC, GPU only.
//!
//! ```no_run
//! use tunnelrez_native::*;
//! // First thing in main(), before any DXGI use (hybrid laptops; see `prefer_power_saving_gpu`).
//! tunnelrez_native::prefer_power_saving_gpu()?;
//! let mut s = NativeSession::start(NativeConfig { output: OutputSel::Virtual, ..Default::default() })?;
//! let config = s.config_bytes(); // -> CONFIG
//! loop {
//!     let au = s.next_au()?; // -> FRAME: t_us = au.capture_t_us, flags bit 0 = au.keyframe
//!     # break;
//! }
//! # anyhow::Ok(())
//! ```
//!
//! Pipeline (TV-measured numbers: PR #3):
//! * capture: DXGI Desktop Duplication (DDA) or Windows.Graphics.Capture (WGC);
//! * colour: D3D11 video processor, BGRA -> NV12 BT.709 limited range, scaled to the encode size;
//! * transfer, when capture runs on another GPU than NVENC (hybrid laptops): D3D12 cross-adapter buffer
//!   (GPU -> system memory -> GPU, no CPU copy) or a mapped staging copy;
//! * encode: NVENC HEVC Main / H.264 High / AV1 Main, ultra-low-latency tuning.

/// Desktop capture back ends (DDA, WGC).
pub mod capture;
/// Sender clock (QPC microseconds).
pub mod clock;
mod conv;
/// GPU colour conversion and scaling.
pub mod csc;
/// DXGI / D3D11 enumeration and device helpers.
pub mod d3d;
/// Per-app GPU preference in the registry.
pub mod gpupref;
/// NVENC encoder.
pub mod nvenc;
mod session;
/// Cross-GPU frame transfer.
pub mod xfer;

pub use nvenc::{Codec, RateMode};
pub use session::{IdrTrigger, NativeSession, PathInfo};

/// Which desktop output to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputSel {
    /// Index into `list_outputs()`.
    Index(usize),
    /// GDI device name, e.g. `\\.\DISPLAY10` (a suffix like `DISPLAY10` also matches).
    Name(String),
    /// The output at desktop position (0,0).
    Primary,
    /// The first Parsec virtual display (see `vdd/`).
    Virtual,
}

/// Which capture API to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureApi {
    /// WGC, else DDA.
    Auto,
    /// DXGI Desktop Duplication (needs the output's owning adapter; no cursor).
    Dda,
    /// Windows.Graphics.Capture (any adapter; includes the cursor).
    Wgc,
}

/// GPU used for WGC capture (DDA always uses the output's owning adapter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureGpu {
    /// The encode GPU (same as `Encode`).
    Auto,
    /// Capture straight on the NVENC GPU (the OS copies each frame across adapters).
    Encode,
    /// The non-NVIDIA GPU.
    Other,
}

/// How frames get from the capture GPU to the encode GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyPath {
    /// Direct if capture and encode GPU are the same, else Staging.
    Auto,
    /// Same GPU: no transfer.
    Direct,
    /// D3D12 cross-adapter shared buffer, GPU to GPU. EXPERIMENTAL, not on any default route (WGC on the NVENC GPU is used instead).
    Cross,
    /// Map on the capture GPU, memcpy, upload to the encode GPU.
    Staging,
}

/// Settings for [`NativeSession::start`].
#[derive(Clone, Debug)]
pub struct NativeConfig {
    /// Desktop output to capture.
    pub output: OutputSel,
    /// Video codec.
    pub codec: Codec,
    /// Encode width; 0 = the output's width. Other sizes are scaled on the GPU (aspect ratio not kept).
    pub width: u32,
    /// Encode height; 0 = the output's height.
    pub height: u32,
    /// Target frame rate; also the pacing limit.
    pub fps: u32,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Capture API.
    pub capture: CaptureApi,
    /// GPU used for WGC capture.
    pub capture_gpu: CaptureGpu,
    /// Capture GPU to encode GPU transfer path.
    pub copy: CopyPath,
    /// NVENC preset 1..=7 (P1 fastest). Default 1.
    pub preset: u8,
    /// Intra refresh period in frames (0 = off; IDR on demand only).
    pub intra_refresh_frames: u32,
    /// VBV size in frames (1.0 = bitrate / fps).
    pub vbv_frames: f32,
    /// Synchronous NVENC (no completion event). Default false = async when the GPU supports it.
    pub sync_encode: bool,
    /// NVENC split-frame encode mode (HEVC, AV1): 0 auto (default), 15 off, 2 two strips (see `EncoderConfig`).
    pub split_mode: u32,
    /// Ceiling (plain CBR, default) or fixed rate (filler pads the stream to `bitrate_bps`).
    pub rate_mode: RateMode,
    /// H.264 `level_idc` (52 = 5.2) high enough for the size, rate and bitrate; None = NVENC autoselect.
    /// Ignored for HEVC.
    pub h264_level: Option<u32>,
}

impl Default for NativeConfig {
    fn default() -> Self {
        NativeConfig {
            output: OutputSel::Virtual,
            codec: Codec::Hevc,
            width: 0,
            height: 0,
            fps: 60,
            bitrate_bps: 30_000_000,
            capture: CaptureApi::Auto,
            capture_gpu: CaptureGpu::Auto,
            copy: CopyPath::Auto,
            preset: 1,
            intra_refresh_frames: 0,
            vbv_frames: 1.0,
            sync_encode: false,
            split_mode: 0,
            rate_mode: RateMode::Ceiling,
            h264_level: None,
        }
    }
}

/// One encoded picture: Annex-B (H.264/HEVC; IDRs include VPS/SPS/PPS in-band) or an AV1 temporal unit
/// (keyframes include the sequence header).
#[derive(Clone, Debug)]
pub struct AccessUnit {
    /// The picture's bytes (Annex-B, or AV1 OBUs).
    pub data: Vec<u8>,
    /// Capture time of the picture on the sender clock (`clock::now_us`, = protocol `t_us`).
    /// For a repeated (static screen) frame: the time it was re-encoded.
    pub capture_t_us: u64,
    /// The picture is an IDR.
    pub keyframe: bool,
    /// When this process received the captured frame.
    pub arrive_t_us: u64,
    /// When the GPU finished the colour conversion (BGRA -> NV12) of it.
    pub convert_done_t_us: u64,
    /// Time of the explicit copy between GPUs for this picture, in microseconds (0: none).
    pub xfer_us: u64,
    /// When NVENC started on it (after colour conversion and transfer).
    pub encode_start_t_us: u64,
    /// When the access unit was ready.
    pub ready_t_us: u64,
    /// Re-encode of the previous picture (no new screen content).
    pub repeat: bool,
}

/// One desktop output as seen by this process.
#[derive(Clone, Debug)]
pub struct OutputInfo {
    /// Position in [`list_outputs`], usable as [`OutputSel::Index`].
    pub index: usize,
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
    /// Description of the adapter that owns the output.
    pub adapter: String,
    /// PCI vendor ID of that adapter.
    pub adapter_vendor: u32,
    /// At desktop position (0,0).
    pub primary: bool,
    /// A Parsec virtual display.
    pub virtual_display: bool,
}

/// All desktop outputs in this process's DXGI view, in adapter then output order.
///
/// # Errors
///
/// DXGI factory or adapter enumeration failed.
pub fn list_outputs() -> anyhow::Result<Vec<OutputInfo>> {
    let v = tunnelrez_vdd::vdd_display_names();
    Ok(d3d::outputs()?
        .into_iter()
        .enumerate()
        .map(|(i, o)| OutputInfo {
            index: i,
            primary: o.x == 0 && o.y == 0,
            virtual_display: v.contains(&o.device_name),
            device_name: o.device_name,
            x: o.x,
            y: o.y,
            width: o.width,
            height: o.height,
            adapter: o.adapter.name,
            adapter_vendor: o.adapter.vendor_id,
        })
        .collect())
}

/// On hybrid laptops, Desktop Duplication only works when this process's DXGI view gives the outputs to the
/// iGPU. This sets the per-exe "power saving" GPU preference for the running executable. DXGI reads it at the
/// first DXGI use in the process, so call this at the very start of `main()`. NVENC still runs on the NVIDIA
/// GPU (it is opened explicitly). Not needed for WGC capture.
///
/// # Errors
///
/// The executable path is unknown, or the registry write failed.
pub fn prefer_power_saving_gpu() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    if gpupref::get(&exe) != Some(gpupref::POWER_SAVING) {
        gpupref::set(&exe, gpupref::POWER_SAVING)?;
    }
    Ok(())
}

/// `D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH` (d3dkmthk.h). REALTIME (5) needs admin rights.
const GPU_PRIORITY_CLASS_HIGH: i32 = 4;

/// Write one best-effort diagnostic line to stderr. This crate has no logger; the host process
/// (sender) owns stderr and forwards it to its own log.
#[expect(clippy::print_stderr, reason = "the crate's only log sink; callers read stderr")]
pub(crate) fn warn(msg: std::fmt::Arguments<'_>) {
    eprintln!("tunnelrez-native: {msg}");
}

/// Raise this process's GPU scheduling class to HIGH (gdi32 `D3DKMTSetProcessSchedulingPriorityClass`), so
/// capture/convert/encode work is scheduled before normal apps on a busy GPU. Best effort; logs failure.
fn raise_gpu_priority() {
    use windows::core::{s, w};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::System::Threading::GetCurrentProcess;
    /// `NTSTATUS D3DKMTSetProcessSchedulingPriorityClass(HANDLE, D3DKMT_SCHEDULINGPRIORITYCLASS)`.
    type SetClassFn = unsafe extern "system" fn(HANDLE, i32) -> i32;

    if std::env::var_os("TUNNELREZ_NO_GPU_PRIORITY").is_some() {
        return; // A/B testing
    }
    // SAFETY: loading gdi32, a system DLL, by a static NUL-terminated name runs no untrusted init code.
    let Ok(lib) = (unsafe { LoadLibraryW(w!("gdi32.dll")) }) else {
        return;
    };
    // SAFETY: `lib` is the module just loaded (never freed); the name is a static NUL-terminated string.
    let Some(f) = (unsafe { GetProcAddress(lib, s!("D3DKMTSetProcessSchedulingPriorityClass")) }) else {
        return;
    };
    // SAFETY: the gdi32 export has exactly the `SetClassFn` signature (d3dkmthk.h); both are `extern "system"` fn pointers.
    let f: SetClassFn = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, SetClassFn>(f) };
    // SAFETY: GetCurrentProcess has no preconditions; it returns a pseudo-handle.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `f` points into gdi32, which stays loaded; it takes the current-process pseudo-handle and a class value.
    let st = unsafe { f(process, GPU_PRIORITY_CLASS_HIGH) };
    if st != 0 {
        warn(format_args!("GPU priority class HIGH not set (NTSTATUS {st:#x})"));
    }
}

/// Timer and power settings for low-latency pacing; runs once per process (see [`init_timer`]).
fn init_timer_once() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE,
    };
    // SAFETY: timeBeginPeriod takes a value only; a matching timeEndPeriod is not needed (process lifetime).
    unsafe { windows::Win32::Media::timeBeginPeriod(1) };
    let st = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: GetCurrentProcess has no preconditions; it returns a pseudo-handle.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: the pointer is to `st`, which lives for the call, and the size passed is exactly its size.
    let _ = unsafe {
        SetProcessInformation(
            process,
            ProcessPowerThrottling,
            std::ptr::from_ref(&st).cast::<std::ffi::c_void>(),
            conv::size_of_u32::<PROCESS_POWER_THROTTLING_STATE>(),
        )
    };
}

/// 1 ms timer resolution, no power throttling, and HIGH GPU scheduling class for this process.
pub fn init_timer() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    static TIMER: Once = Once::new();
    ONCE.call_once(raise_gpu_priority);
    TIMER.call_once(init_timer_once);
}
