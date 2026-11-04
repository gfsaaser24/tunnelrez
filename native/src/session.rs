//! `NativeSession`: capture -> (colour convert + scale) -> (cross-GPU transfer) -> NVENC, with frame pacing.
//!
//! Robustness: a display mode change re-creates only the colour converter (the stream keeps its size; the new
//! source is scaled into it). Lost capture (DDA access lost, secure desktop, display unplugged) repeats the
//! last picture for up to 3 s while capture is re-created, then fails so the caller can start a new session.
//! GPU errors (device removed) are returned as errors as well.

use crate::capture::{dda::DdaCapture, wgc::WgcCapture};
use crate::csc::{create_tex, Csc};
use crate::d3d::{self, Device};
use crate::nvenc::{Encoder, EncoderConfig, RateMode};
use crate::xfer::{self, Xfer};
use crate::{clock, AccessUnit, CaptureApi, CaptureGpu, CopyPath, NativeConfig, OutputSel};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_FENCE_FLAG_NONE, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// No new content for this long = static screen: start repeating the last picture at the stream rate.
const REPEAT_AFTER_US: u64 = 100_000;

/// Content counts as moving after this many new pictures in a row, each within [`MOVING_GAP_PERIODS`]
/// frame periods of the one before (scrolling, video). Fewer = sporadic (a blinking caret, a clock).
const MOVING_FRAMES: u32 = 3;

/// Largest gap, in frame periods, between two new pictures of moving content.
const MOVING_GAP_PERIODS: u64 = 3;

/// The moving streak after a new picture at `now` (µs) whose predecessor came at `last` (0 = none).
///
/// A gap counts as moving up to [`MOVING_GAP_PERIODS`] periods plus half a period: `period_us` is truncated
/// (16 666 at 60 fps) and capture times jitter, so a steady 20 fps update (50 000 µs) must still count.
fn next_moving_streak(streak: u32, last: u64, now: u64, period_us: u64) -> u32 {
    let limit = MOVING_GAP_PERIODS * period_us + period_us / 2;
    if last != 0 && now.saturating_sub(last) <= limit {
        streak.saturating_add(1)
    } else {
        0
    }
}

/// How long (µs) after the last access unit the last picture is re-encoded when no new content came.
///
/// While repeating, every period. Otherwise [`REPEAT_AFTER_US`] (at least 1.5 periods), so a repeat never
/// lands between the pictures of moving content. With [`RateMode::Fixed`] and sporadic content, 1.5
/// periods: each sporadic update would otherwise cost ~100 ms of frames, and with them their share of the
/// fixed rate (a caret blinking twice a second cut a 20 Mb/s stream to 16).
fn repeat_wait_us(period_us: u64, repeat_streak: u32, moving_streak: u32, rate: RateMode) -> u64 {
    let after_content = period_us * 3 / 2;
    if repeat_streak > 0 {
        period_us
    } else if rate == RateMode::Fixed && moving_streak < MOVING_FRAMES {
        after_content
    } else {
        REPEAT_AFTER_US.max(after_content)
    }
}

enum Cap {
    Wgc(WgcCapture),
    Dda(DdaCapture),
}

/// Requests an IDR from any thread (e.g. the network thread on `IDR_REQUEST`).
#[derive(Clone, Debug, Default)]
pub struct IdrTrigger(Arc<AtomicBool>);

impl IdrTrigger {
    /// Make the session's next access unit an IDR.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// What the session ended up using (for logs / STATS).
#[derive(Clone, Debug)]
pub struct PathInfo {
    /// Capture API in use (never `Auto`).
    pub capture: CaptureApi,
    /// Name of the adapter that captures.
    pub capture_gpu: String,
    /// Name of the adapter NVENC runs on.
    pub encode_gpu: String,
    /// Transfer between the two GPUs (never `Auto`).
    pub copy: CopyPath,
    /// GDI device name of the captured output.
    pub output: String,
    /// Captured size in pixels (width, height) at start.
    pub source_size: (u32, u32),
    /// Encoded size in pixels (width, height).
    pub encode_size: (u32, u32),
    /// NVENC runs in async mode (completion event).
    pub async_encode: bool,
    /// Number of NVENC engines on the encode GPU.
    pub nvenc_engines: i32,
}

/// Fence + event: learn on the CPU when the GPU finished the work queued so far.
struct GpuDone {
    ctx4: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    event: HANDLE,
    v: u64,
}

impl GpuDone {
    fn new(dev: &Device) -> Result<Self> {
        let d5: ID3D11Device5 = dev.device.cast()?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: CreateFence on a live device writes only the local out-param.
        unsafe { d5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &raw mut fence) }?;
        let ctx4: ID3D11DeviceContext4 = dev.ctx.cast()?;
        let fence = fence.ok_or_else(|| anyhow!("no fence"))?;
        // SAFETY: an unnamed auto-reset event with default security; closed in Drop.
        let event = unsafe { CreateEventW(None, false, false, None) }?;
        Ok(GpuDone { ctx4, fence, event, v: 0 })
    }

    /// Signal after everything queued so far, flush, and wait for it.
    fn wait(&mut self) -> Result<()> {
        self.v += 1;
        // SAFETY: the fence was created on this context's device; the context is used from this thread only.
        unsafe { self.ctx4.Signal(&self.fence, self.v) }?;
        // SAFETY: the event handle belongs to this struct and stays open until Drop.
        unsafe { self.fence.SetEventOnCompletion(self.v, self.event) }?;
        // SAFETY: Flush takes no arguments; the context is used from this thread only.
        unsafe { self.ctx4.Flush() };
        // SAFETY: the event handle belongs to this struct and stays open until Drop.
        if unsafe { WaitForSingleObject(self.event, 500) } != WAIT_OBJECT_0 {
            bail!("GPU did not finish the colour conversion in 500 ms (device lost?)");
        }
        Ok(())
    }
}

impl Drop for GpuDone {
    fn drop(&mut self) {
        // SAFETY: the event was created in `new` and is closed only here.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

/// Capture-side state that converts captured textures into the NV12 frame.
struct Ingest {
    dev: Device,
    csc: Csc,
    /// NV12 conversion output (on `dev`).
    out: ID3D11Texture2D,
    fps: u32,
    /// BGRA copy of the frame for drivers whose video processor rejects the capture texture directly.
    vp_in: Option<ID3D11Texture2D>,
    done: GpuDone,
}

impl Ingest {
    /// Convert `tex` into the NV12 frame and wait until the GPU has done it.
    fn convert(&mut self, tex: &ID3D11Texture2D) -> Result<()> {
        let d = d3d::tex_desc(tex);
        if d.Width != self.csc.in_w || d.Height != self.csc.in_h {
            // Mode change: keep the encoder (same stream size) and scale the new source into it.
            crate::warn(format_args!("source {}x{} -> {}x{}", self.csc.in_w, self.csc.in_h, d.Width, d.Height));
            self.csc = Csc::new(&self.dev, d.Width, d.Height, &self.out, self.fps)?;
            self.vp_in = None;
        }
        let both = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0).cast_unsigned();
        // Intel's video processor silently outputs black for SRV+RTV input textures (measured); copy first.
        let input = if self.dev.adapter.vendor_id == d3d::VENDOR_INTEL && d.BindFlags & both == both {
            let t = if let Some(t) = &self.vp_in {
                t.clone()
            } else {
                let t =
                    create_tex(&self.dev, d.Width, d.Height, d.Format, D3D11_BIND_RENDER_TARGET.0.cast_unsigned(), 0)?;
                self.vp_in = Some(t.clone());
                t
            };
            // SAFETY: both textures belong to the capture device, whose context is used from this thread only.
            unsafe { self.dev.ctx.CopyResource(&t, tex) };
            t
        } else {
            tex.clone()
        };
        self.csc.convert(&input)?;
        self.done.wait()
    }
}

/// A converted frame waiting for the encoder. All times are `clock::now_us` microseconds.
#[derive(Clone, Copy)]
struct Pending {
    capture: u64,
    arrive: u64,
    convert_done: u64,
}

/// Field order is drop order: the encoder (and its registered input) goes before the textures and devices.
pub struct NativeSession {
    enc: Encoder,
    xfer: Option<Xfer>,
    ingest: Ingest,
    cap: Cap,
    idr: IdrTrigger,
    period_us: u64,
    /// Rate limiter for new content: a token bucket in µs (one period per AU, at most one period of credit),
    /// so a frame that arrives a little early is encoded at once instead of waiting for a fixed gap.
    tokens_us: i64,
    tokens_at: u64,
    last_enc_us: u64,
    pending: Option<Pending>,
    have_frame: bool,
    repeat_streak: u32,
    /// New pictures in a row that came within `MOVING_GAP_PERIODS` periods of the one before.
    moving_streak: u32,
    /// Encode start of the last new (not repeated) picture.
    last_content_us: u64,
    rate_mode: RateMode,
    frame_no: u64,
    info: PathInfo,
    /// Since when capture has been lost (and not re-created yet).
    lost_since: Option<Instant>,
    /// DDA probe counters (images vs. pointer-only updates).
    dda_images: u32,
    dda_other: u32,
}

// SAFETY: NativeSession exclusively owns every COM object, handle and NVENC session inside it. It is created on one thread and then moved to, and used only on, the sender's encode thread; only the IdrTrigger atomic is shared with other threads.
unsafe impl Send for NativeSession {}

impl std::fmt::Debug for NativeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSession")
            .field("info", &self.info)
            .field("period_us", &self.period_us)
            .field("frame_no", &self.frame_no)
            .field("have_frame", &self.have_frame)
            .finish_non_exhaustive()
    }
}

fn pick_output(sel: &OutputSel, outs: &[d3d::Output]) -> Result<d3d::Output> {
    let o = match sel {
        OutputSel::Index(i) => outs.get(*i).cloned(),
        OutputSel::Name(n) => outs
            .iter()
            .find(|o| {
                o.device_name.eq_ignore_ascii_case(n)
                    || o.device_name.to_ascii_uppercase().ends_with(&n.to_ascii_uppercase())
            })
            .cloned(),
        OutputSel::Primary => outs.iter().find(|o| o.x == 0 && o.y == 0).cloned(),
        OutputSel::Virtual => {
            let v = tunnelrez_vdd::vdd_display_names();
            outs.iter().find(|o| v.contains(&o.device_name)).cloned()
        }
    };
    o.ok_or_else(|| anyhow!("output {:?} not found ({} outputs)", sel, outs.len()))
}

fn same_adapter(a: &Device, b: &Device) -> bool {
    same_luid(&a.adapter, &b.adapter)
}

/// Open the capture for `out` with `api` (WGC or DDA) and the device it runs on.
fn open_capture(
    cfg: &NativeConfig,
    api: CaptureApi,
    out: &d3d::Output,
    enc_dev: &Device,
    other: Option<&d3d::Adapter>,
) -> Result<(Cap, Device)> {
    if api == CaptureApi::Dda {
        // DDA needs the adapter that owns the output in this process's view (see `gpupref`).
        let dev =
            if same_luid(&out.adapter, &enc_dev.adapter) { enc_dev.clone() } else { d3d::create_device(&out.adapter)? };
        let c = DdaCapture::new(&dev.device, &out.output).with_context(|| format!("DDA on {}", out.device_name))?;
        Ok((Cap::Dda(c), dev))
    } else {
        let dev = match (cfg.capture_gpu, other) {
            (CaptureGpu::Auto | CaptureGpu::Encode, _) | (_, None) => enc_dev.clone(),
            (CaptureGpu::Other, Some(a)) => d3d::create_device(a)?,
        };
        let c = WgcCapture::new(&dev.device, out.monitor).with_context(|| format!("WGC on {}", out.device_name))?;
        Ok((Cap::Wgc(c), dev))
    }
}

/// The cross-GPU transfer for `want` (Staging or Cross); with `copy = Auto` a failed Cross falls back to Staging.
fn open_xfer(
    copy: CopyPath,
    want: CopyPath,
    cap: (&Device, &ID3D11Texture2D),
    enc: (&Device, &ID3D11Texture2D),
) -> Result<Xfer> {
    let kind = if want == CopyPath::Staging { xfer::Kind::Staging } else { xfer::Kind::Cross };
    match Xfer::new(kind, cap.0, cap.1, enc.0, enc.1) {
        Ok(x) => Ok(x),
        Err(e) if copy == CopyPath::Auto => {
            crate::warn(format_args!("cross-adapter copy unavailable ({e:#}); using the staging copy"));
            Xfer::new(xfer::Kind::Staging, cap.0, cap.1, enc.0, enc.1)
        }
        Err(e) => Err(e),
    }
}

/// Two adapters are the same GPU (same LUID).
fn same_luid(a: &d3d::Adapter, b: &d3d::Adapter) -> bool {
    d3d::luid_u64(a.luid) == d3d::luid_u64(b.luid)
}

impl NativeSession {
    /// Start capturing and encoding. `CaptureApi::Auto` = WGC on the NVENC GPU (zero-copy when that GPU
    /// renders the output, e.g. the Parsec virtual display; the OS copies across adapters otherwise), then DDA.
    ///
    /// # Errors
    ///
    /// No NVIDIA GPU, output not found, capture or NVENC setup failed.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "public signature used by the sender, which hands over an owned config"
    )]
    pub fn start(cfg: NativeConfig) -> Result<Self> {
        crate::init_timer();
        match cfg.capture {
            CaptureApi::Auto => match Self::start_with(&cfg, CaptureApi::Wgc) {
                Ok(s) => Ok(s),
                Err(e) => {
                    Self::start_with(&cfg, CaptureApi::Dda).with_context(|| format!("WGC failed ({e:#}), then DDA"))
                }
            },
            api => Self::start_with(&cfg, api),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one setup sequence in order: device, capture, conversion, encoder; split, the order is harder to see"
    )]
    fn start_with(cfg: &NativeConfig, api: CaptureApi) -> Result<Self> {
        let outs = d3d::outputs()?;
        let out = pick_output(&cfg.output, &outs)?;
        let nv = d3d::adapter_by_vendor(d3d::VENDOR_NVIDIA).context("NVENC needs an NVIDIA GPU")?;
        let enc_dev = d3d::create_device(&nv)?;

        let other = d3d::adapters()?.into_iter().find(|a| !a.software && a.vendor_id != d3d::VENDOR_NVIDIA);
        let (cap, cap_dev) = open_capture(cfg, api, &out, &enc_dev, other.as_ref())?;
        let (sw, sh) = match &cap {
            Cap::Wgc(c) => c.size(),
            Cap::Dda(c) => (c.width, c.height),
        };
        let (w, h) = (if cfg.width == 0 { sw } else { cfg.width }, if cfg.height == 0 { sh } else { cfg.height });
        if w % 2 != 0 || h % 2 != 0 {
            bail!("encode size must be even ({w}x{h})");
        }

        let same = same_adapter(&cap_dev, &enc_dev);
        let want = match (same, cfg.copy) {
            (true, _) => CopyPath::Direct,
            (false, CopyPath::Direct) => bail!("copy=direct needs capture on the encode GPU"),
            (false, CopyPath::Auto) => CopyPath::Staging, // Cross is experimental (not on a default route)
            (false, c) => c,
        };
        let rt = D3D11_BIND_RENDER_TARGET.0.cast_unsigned();
        let misc = if want == CopyPath::Cross {
            (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0).cast_unsigned()
        } else {
            0
        };
        let nv12_cap = create_tex(&cap_dev, w, h, DXGI_FORMAT_NV12, rt, misc)?;
        let nv12_enc = if same { nv12_cap.clone() } else { create_tex(&enc_dev, w, h, DXGI_FORMAT_NV12, rt, misc)? };
        let xfer =
            if same { None } else { Some(open_xfer(cfg.copy, want, (&cap_dev, &nv12_cap), (&enc_dev, &nv12_enc))?) };
        let copy = match &xfer {
            None => CopyPath::Direct,
            Some(x) if x.kind() == xfer::Kind::Cross => CopyPath::Cross,
            Some(_) => CopyPath::Staging,
        };
        let csc = Csc::new(&cap_dev, sw, sh, &nv12_cap, cfg.fps)?;
        let enc = Encoder::new(
            &enc_dev.device,
            &nv12_enc,
            EncoderConfig {
                codec: cfg.codec,
                width: w,
                height: h,
                fps: cfg.fps,
                bitrate_bps: cfg.bitrate_bps,
                preset: cfg.preset,
                vbv_frames: cfg.vbv_frames,
                intra_refresh_frames: cfg.intra_refresh_frames,
                two_pass: false,
                sync: cfg.sync_encode,
                split_mode: cfg.split_mode,
                rate_mode: cfg.rate_mode,
                h264_level: cfg.h264_level,
            },
        )?;
        let period_us = 1_000_000 / u64::from(cfg.fps.max(1));
        let info = PathInfo {
            capture: if matches!(cap, Cap::Dda(_)) { CaptureApi::Dda } else { CaptureApi::Wgc },
            capture_gpu: cap_dev.adapter.name.clone(),
            encode_gpu: enc_dev.adapter.name,
            copy,
            output: out.device_name,
            source_size: (sw, sh),
            encode_size: (w, h),
            async_encode: enc.is_async(),
            nvenc_engines: enc.engines(),
        };
        let mut s = NativeSession {
            cap,
            ingest: Ingest {
                done: GpuDone::new(&cap_dev)?,
                dev: cap_dev,
                csc,
                out: nv12_cap,
                fps: cfg.fps,
                vp_in: None,
            },
            xfer,
            enc,
            idr: IdrTrigger::default(),
            period_us,
            tokens_us: period_us.cast_signed(),
            tokens_at: clock::now_us(),
            last_enc_us: 0,
            pending: None,
            have_frame: false,
            repeat_streak: 0,
            moving_streak: 0,
            last_content_us: 0,
            rate_mode: cfg.rate_mode,
            frame_no: 0,
            info,
            lost_since: None,
            dda_images: 0,
            dda_other: 0,
        };
        if matches!(s.cap, Cap::Dda(_)) {
            s.probe_dda()?;
        }
        Ok(s)
    }

    /// On a hybrid laptop in a high-performance process, DDA "works" on the dGPU but never delivers a desktop
    /// image for outputs the iGPU renders. Wait for the first image (DDA always delivers one right after
    /// `DuplicateOutput`) and fail if only empty updates arrive.
    fn probe_dda(&mut self) -> Result<()> {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(500) {
            if self.poll_capture(50)? {
                return Ok(());
            }
            if self.dda_other >= 8 {
                break;
            }
        }
        if self.dda_other > 0 && self.dda_images == 0 {
            bail!("DDA delivers no desktop images on this adapter (hybrid GPU: set the power-saving GPU preference, see gpupref)");
        }
        Ok(()) // static desktop: no updates at all yet
    }

    /// What the session ended up using (capture API, GPUs, transfer, sizes).
    pub fn info(&self) -> &PathInfo {
        &self.info
    }

    /// Annex-B VPS/SPS/PPS (HEVC) or SPS/PPS (H.264), or the AV1 sequence header OBU, for the CONFIG message.
    /// Keyframes carry them too.
    pub fn config_bytes(&self) -> Vec<u8> {
        self.enc.config_bytes().to_vec()
    }

    /// The next access unit will be an IDR.
    pub fn force_idr(&self) {
        self.idr.request();
    }

    /// A handle that can request an IDR from another thread.
    pub fn idr_trigger(&self) -> IdrTrigger {
        self.idr.clone()
    }

    /// Wait up to `timeout_ms` for new capture content. True if a new frame was converted.
    fn poll_capture(&mut self, timeout_ms: u32) -> Result<bool> {
        let lost = match &self.cap {
            Cap::Wgc(c) => c.closed(),
            Cap::Dda(c) => c.lost(),
        };
        match (lost, self.lost_since) {
            (true, None) => {
                crate::warn(format_args!("capture lost (mode change, secure desktop or display removed); waiting"));
                self.lost_since = Some(Instant::now());
            }
            (true, Some(t)) if t.elapsed() > Duration::from_secs(3) => {
                bail!("capture lost for 3 s (display removed?)")
            }
            (false, Some(_)) => {
                crate::warn(format_args!("capture back"));
                self.lost_since = None;
                self.idr.request();
            }
            _ => {}
        }
        let p = match &mut self.cap {
            Cap::Wgc(c) => match c.next(timeout_ms)? {
                Some(f) => {
                    let arrive = clock::now_us();
                    // The pool buffer stays locked (f alive) until the conversion is done.
                    self.ingest.convert(&f.tex)?;
                    Pending { capture: f.t_us.min(arrive), arrive, convert_done: clock::now_us() }
                }
                None => return Ok(false),
            },
            Cap::Dda(c) => match c.next(timeout_ms)? {
                Some(f) if f.new_image => {
                    self.dda_images += 1;
                    let arrive = clock::now_us();
                    self.ingest.convert(&f.tex)?;
                    Pending { capture: f.t_us.min(arrive), arrive, convert_done: clock::now_us() }
                }
                Some(_) => {
                    self.dda_other += 1;
                    return Ok(false);
                }
                None => return Ok(false),
            },
        };
        self.pending = Some(p);
        Ok(true)
    }

    /// Block until the next access unit is ready.
    ///
    /// # Errors
    ///
    /// As [`NativeSession::poll_au`].
    pub fn next_au(&mut self) -> Result<AccessUnit> {
        loop {
            if let Some(au) = self.poll_au(100)? {
                return Ok(au);
            }
        }
    }

    /// The next access unit, or `Ok(None)` if none is due within about `timeout_ms` (only before the first
    /// captured frame: after that the last picture is repeated at the stream rate).
    ///
    /// Pacing: new content is encoded as soon as it is converted, limited to `fps` on average by a token
    /// bucket (half a period of early slack, so 60 Hz capture jitter never makes a frame wait; a faster
    /// display is sampled at `fps`). When the screen is static the last frame is
    /// re-encoded (tiny P-frame, or a filler-padded one at a fixed rate) after `REPEAT_AFTER_US` and then
    /// every period (see [`repeat_wait_us`]). No repeats while content moves (a repeat squeezed into a
    /// capture gap made the TV drop the next real frame as late).
    ///
    /// # Errors
    ///
    /// Capture lost for more than 3 s, GPU work timed out (device lost), or NVENC failed. The session is
    /// not usable after an error; start a new one.
    pub fn poll_au(&mut self, timeout_ms: u32) -> Result<Option<AccessUnit>> {
        let t0 = clock::now_us();
        loop {
            let now = clock::now_us();
            let since = now.saturating_sub(self.last_enc_us);
            let gap = if self.pending.is_some() {
                self.content_wait_us(now)
            } else if self.have_frame && self.idr.0.load(Ordering::SeqCst) {
                0 // IDR requested: re-encode the last picture as an IDR now
            } else if self.have_frame {
                repeat_wait_us(self.period_us, self.repeat_streak, self.moving_streak, self.rate_mode)
            } else {
                u64::MAX
            };
            if since >= gap {
                // An overdue repeat or IDR re-encode must not starve capture: when encode + send take a whole
                // period, `since >= gap` holds on every call and the poll below is never reached. Check for new
                // content (and capture loss) without waiting; new content then goes through the token bucket.
                if self.pending.is_none() && self.have_frame && self.poll_capture(0)? {
                    continue;
                }
                return self.encode_now().map(Some);
            }
            let left_ms = (u64::from(timeout_ms) * 1000).saturating_sub(now - t0) / 1000;
            if gap == u64::MAX && left_ms == 0 {
                return Ok(None);
            }
            let wait_ms = if gap == u64::MAX {
                u32::try_from(left_ms.min(100)).unwrap_or(100)
            } else {
                u32::try_from((gap - since).div_ceil(1000)).unwrap_or(u32::MAX)
            };
            if self.poll_capture(wait_ms.max(1))? && self.content_wait_us(clock::now_us()) == 0 {
                return self.encode_now().map(Some);
            }
        }
    }

    /// Refill the token bucket; how long (µs) new content must wait, as a "since last AU" threshold.
    fn content_wait_us(&mut self, now: u64) -> u64 {
        let p = self.period_us.cast_signed();
        self.tokens_us = (self.tokens_us + now.saturating_sub(self.tokens_at).cast_signed()).min(p);
        self.tokens_at = now;
        let need = p / 2 - self.tokens_us; // allowed when tokens >= period/2
        let since = now.saturating_sub(self.last_enc_us);
        if need <= 0 {
            0
        } else {
            since + need.cast_unsigned()
        }
    }

    fn encode_now(&mut self) -> Result<AccessUnit> {
        let mut xfer_us = 0;
        let (p, repeat) = if let Some(p) = self.pending.take() {
            if let Some(x) = &mut self.xfer {
                let t0 = clock::now_us();
                x.run()?;
                xfer_us = clock::now_us().saturating_sub(t0);
            }
            self.have_frame = true;
            (p, false)
        } else {
            let n = clock::now_us();
            (Pending { capture: n, arrive: n, convert_done: n }, true)
        };
        self.repeat_streak = if repeat { self.repeat_streak + 1 } else { 0 };
        let start = clock::now_us();
        if !repeat {
            self.moving_streak = next_moving_streak(self.moving_streak, self.last_content_us, start, self.period_us);
            self.last_content_us = start;
        }
        self.last_enc_us = start;
        self.content_wait_us(start);
        self.tokens_us -= self.period_us.cast_signed();
        let idr = self.idr.0.swap(false, Ordering::SeqCst) || self.frame_no == 0;
        let f = self.enc.encode(idr, self.frame_no)?;
        self.frame_no += 1;
        Ok(AccessUnit {
            data: f.data,
            capture_t_us: p.capture,
            keyframe: f.keyframe,
            arrive_t_us: p.arrive,
            convert_done_t_us: p.convert_done,
            xfer_us,
            encode_start_t_us: start,
            ready_t_us: clock::now_us(),
            repeat,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 16_666;

    #[test]
    fn repeating_stays_at_the_frame_rate() {
        for rate in [RateMode::Ceiling, RateMode::Fixed] {
            assert_eq!(repeat_wait_us(P, 1, 0, rate), P);
            assert_eq!(repeat_wait_us(P, 9, MOVING_FRAMES, rate), P);
        }
    }

    #[test]
    fn ceiling_waits_100_ms_after_content() {
        assert_eq!(repeat_wait_us(P, 0, 0, RateMode::Ceiling), REPEAT_AFTER_US);
        assert_eq!(repeat_wait_us(P, 0, MOVING_FRAMES, RateMode::Ceiling), REPEAT_AFTER_US);
    }

    #[test]
    fn fixed_rate_repeats_soon_after_sporadic_content_but_not_between_moving_frames() {
        assert_eq!(repeat_wait_us(P, 0, 0, RateMode::Fixed), P * 3 / 2, "caret blink: keep the rate");
        assert_eq!(repeat_wait_us(P, 0, MOVING_FRAMES - 1, RateMode::Fixed), P * 3 / 2);
        assert_eq!(repeat_wait_us(P, 0, MOVING_FRAMES, RateMode::Fixed), REPEAT_AFTER_US, "moving: no repeats");
        // A 60 Hz frame (one period) always comes before the 1.5-period repeat.
        assert!(repeat_wait_us(P, 0, 0, RateMode::Fixed) > P);
    }

    /// Feed new-picture times through `next_moving_streak`; the streak after each one.
    fn streaks(times: &[u64]) -> Vec<u32> {
        let (mut s, mut last) = (0, 0);
        times
            .iter()
            .map(|&t| {
                s = next_moving_streak(s, last, t, P);
                last = t;
                s
            })
            .collect()
    }

    #[test]
    fn steady_20_fps_and_faster_counts_as_moving() {
        for gap in [16_667, 33_333, 50_000, 50_400] {
            let times: Vec<u64> = (1..=6).map(|i| i * gap).collect();
            let s = streaks(&times);
            assert!(s[usize::try_from(MOVING_FRAMES).expect("small")] >= MOVING_FRAMES, "gap {gap}: {s:?}");
            assert_eq!(
                repeat_wait_us(P, 0, *s.last().unwrap(), RateMode::Fixed),
                REPEAT_AFTER_US,
                "gap {gap}: no repeats"
            );
        }
    }

    #[test]
    fn sporadic_updates_stay_sporadic() {
        let s = streaks(&[1_000, 501_000, 1_001_000, 1_070_000, 1_570_000]);
        assert_eq!(s, [0, 0, 0, 0, 0], "caret blink / 70 ms gaps are not motion");
    }

    #[test]
    fn low_frame_rates_never_repeat_faster_than_1_5_periods() {
        let p30 = 33_333;
        assert_eq!(repeat_wait_us(p30, 0, 0, RateMode::Fixed), p30 * 3 / 2);
        assert_eq!(repeat_wait_us(200_000, 0, MOVING_FRAMES, RateMode::Ceiling), 300_000);
    }
}
