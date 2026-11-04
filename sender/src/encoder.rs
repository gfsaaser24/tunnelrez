//! Encoder backends for the ffmpeg path, and the plan (capture view + filter chain + encoder
//! options) for one ffmpeg process.
//!
//! Backends, in `--encoder auto` order: NVENC, Intel `QuickSync`, AMD AMF, Windows Media Foundation
//! (any GPU vendor's MFT, incl. ARM64; more latency), then software (x264 / x265 / SVT-AV1).
//! Every backend gets low-latency settings: no B-frames, CBR with a ~1 frame VBV, long GOP with
//! IDR on request.

use crate::flv::Codec;
use crate::level;
use crate::monitors::{VENDOR_AMD, VENDOR_INTEL, VENDOR_NVIDIA};
use crate::num::{f64_from_u64, u64_sat_from_f64};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Backend {
    Nvenc,
    Qsv,
    Amf,
    Mf,
    X264,
    X265,
    SvtAv1,
}

pub const ALL: [Backend; 7] =
    [Backend::Nvenc, Backend::Qsv, Backend::Amf, Backend::Mf, Backend::X264, Backend::X265, Backend::SvtAv1];

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Nvenc => "nvenc",
            Backend::Qsv => "qsv",
            Backend::Amf => "amf",
            Backend::Mf => "mf",
            Backend::X264 => "x264",
            Backend::X265 => "x265",
            Backend::SvtAv1 => "svtav1",
        }
    }

    pub fn parse(s: &str) -> Option<Backend> {
        Some(match s.to_ascii_lowercase().as_str() {
            "nvenc" | "nvidia" => Backend::Nvenc,
            "qsv" | "quicksync" | "intel" => Backend::Qsv,
            "amf" | "amd" => Backend::Amf,
            "mf" | "mediafoundation" => Backend::Mf,
            "x264" | "libx264" => Backend::X264,
            "x265" | "libx265" => Backend::X265,
            "svtav1" | "svt-av1" | "libsvtav1" => Backend::SvtAv1,
            _ => return None,
        })
    }

    /// ffmpeg encoder name for `c`, or None if this backend does not do that codec.
    pub fn encoder(self, c: Codec) -> Option<&'static str> {
        use Codec::*;
        Some(match (self, c) {
            (Backend::Nvenc, H264) => "h264_nvenc",
            (Backend::Nvenc, Hevc) => "hevc_nvenc",
            (Backend::Nvenc, Av1) => "av1_nvenc",
            (Backend::Qsv, H264) => "h264_qsv",
            (Backend::Qsv, Hevc) => "hevc_qsv",
            // av1_qsv (ffmpeg 8.1) rejects forced keyframes ("Invalid FrameType:0"), so no AV1 here.
            (Backend::Amf, H264) => "h264_amf",
            (Backend::Amf, Hevc) => "hevc_amf",
            (Backend::Amf, Av1) => "av1_amf",
            (Backend::Mf, H264) => "h264_mf",
            (Backend::Mf, Hevc) => "hevc_mf",
            (Backend::X264, H264) => "libx264",
            (Backend::X265, Hevc) => "libx265",
            (Backend::SvtAv1, Av1) => "libsvtav1",
            _ => return None,
        })
    }

    /// GPU vendor whose hardware this backend drives (None: any GPU / CPU).
    pub fn vendor(self) -> Option<u32> {
        match self {
            Backend::Nvenc => Some(VENDOR_NVIDIA),
            Backend::Qsv => Some(VENDOR_INTEL),
            Backend::Amf => Some(VENDOR_AMD),
            _ => None,
        }
    }

    pub fn software(self) -> bool {
        matches!(self, Backend::X264 | Backend::X265 | Backend::SvtAv1)
    }
}

/// The software encoder for a codec.
pub fn software_for(c: Codec) -> Backend {
    match c {
        Codec::H264 => Backend::X264,
        Codec::Hevc => Backend::X265,
        Codec::Av1 => Backend::SvtAv1,
    }
}

/// `--encoder auto` order for a codec: hardware first, then Media Foundation, then software.
pub fn auto_order(c: Codec) -> Vec<Backend> {
    let mut v: Vec<Backend> = [Backend::Nvenc, Backend::Qsv, Backend::Amf, Backend::Mf]
        .into_iter()
        .filter(|b| b.encoder(c).is_some())
        .collect();
    v.push(software_for(c));
    v
}

/// What the target bitrate means to the encoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RateMode {
    /// CBR as a ceiling: simple or static content sends less than the target.
    #[default]
    Ceiling,
    /// The stream really sends the target: rate control may use a low min QP (quality first), then filler
    /// data pads each frame to its share of the rate. Every frame is full size, so each takes longer on
    /// the network (150 Mb/s at 60 fps: ~312 KB per frame, ~2.5 ms on gigabit, ~8 ms on 300 Mb/s Wi-Fi).
    /// Padding: NVENC (`-cbr_padding`), AMF (`-filler_data`), x264 (`-nal-hrd cbr`). QSV, MF, x265 and
    /// SVT-AV1 have no padding option and keep acting as a ceiling.
    Fixed,
}

impl RateMode {
    pub fn name(self) -> &'static str {
        match self {
            RateMode::Ceiling => "ceiling",
            RateMode::Fixed => "fixed",
        }
    }
}

/// Lowest QP rate control may use for [`RateMode::Fixed`] (NVENC `-qmin`, same as the native backend).
pub const FIXED_MIN_QP: u32 = 1;

/// The target for `codec`: `asked` as given (every codec), else the default for the codec and size.
pub fn target_bitrate(asked: Option<u64>, codec: Codec, w: u32, h: u32, fps: u32) -> u64 {
    asked.unwrap_or_else(|| default_bitrate(codec, w, h, fps))
}

// PINNED: see docs/PINNED.md (FFmpeg 7+; 8+ for NVENC padding)
/// Whether this ffmpeg's NVENC encoders take `-cbr_padding` (ffmpeg 8+).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NvencPadding {
    Supported,
    Missing,
}

/// The rate mode `backend` really delivers when `want` is asked for: [`RateMode::Fixed`] only where the
/// encoder can pad (NVENC with an ffmpeg that has `-cbr_padding`, AMF, x264). Everything else is a ceiling,
/// and is reported as one so the app does not warn about normal undershoot.
pub fn effective_rate(want: RateMode, backend: Backend, nvenc: NvencPadding) -> RateMode {
    let pads = match backend {
        Backend::Nvenc => nvenc == NvencPadding::Supported,
        Backend::Amf | Backend::X264 => true,
        Backend::Qsv | Backend::Mf | Backend::X265 | Backend::SvtAv1 => false,
    };
    if want == RateMode::Fixed && pads {
        RateMode::Fixed
    } else {
        RateMode::Ceiling
    }
}

/// Default bitrate: 1080p60 -> 4K60 per codec (H.264 30 -> 60 Mb/s; HEVC 20 -> 100; AV1 16 -> 100),
/// linear in pixel rate in between, square-root outside.
pub fn default_bitrate(codec: Codec, w: u32, h: u32, fps: u32) -> u64 {
    let (lo, hi) = match codec {
        Codec::H264 => (30e6, 60e6),
        Codec::Hevc => (20e6, 100e6),
        Codec::Av1 => (16e6, 100e6),
    };
    let px_rate = (f64::from(w) * f64::from(h) * f64::from(fps)) / (1920.0 * 1080.0 * 60.0);
    let rate = if px_rate <= 1.0 {
        lo * px_rate.sqrt()
    } else if px_rate <= 4.0 {
        lo + (hi - lo) * (px_rate - 1.0) / 3.0
    } else {
        hi * (px_rate / 4.0).sqrt()
    };
    u64_sat_from_f64((rate.max(2e6) / 1e5).round()) * 100_000
}

/// Software encoder threads: half the logical CPUs, 2..=8 (x264 sliced threads stop scaling
/// past ~8 at 1080p, and the rest of the machine stays responsive).
pub fn default_threads() -> u32 {
    let n = std::thread::available_parallelism().map_or(4, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    (n / 2).clamp(2, 8)
}

/// Which helper process (GPU preference) runs ffmpeg. On a hybrid laptop this decides which
/// adapter owns the outputs, so which GPU ddagrab captures on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum GpuPref {
    PowerSaving,
    HighPerformance,
}

impl GpuPref {
    pub fn name(self) -> &'static str {
        match self {
            GpuPref::PowerSaving => "igpu",
            GpuPref::HighPerformance => "dgpu",
        }
    }
}

/// One way to capture the chosen monitor: ddagrab `output_idx` on `vendor`'s adapter, inside a
/// process with GPU preference `pref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub pref: GpuPref,
    pub output_idx: u32,
    pub vendor: u32,
    pub adapter: String,
    pub width: u32,
    pub height: u32,
}

/// How frames get from the capture GPU to the encoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Path {
    /// Capture texture -> encoder on the same GPU, no system-memory copy.
    ZeroCopy,
    /// GPU convert/scale to NV12, download to system memory, (upload to the encoder GPU).
    Copy,
    /// Copy path, but the NV12 frame is uploaded to CUDA at capture size and scaled there (NVENC
    /// only; moves fewer bytes when the output is larger than the capture).
    CopyScaleCuda,
}

impl Path {
    pub fn name(self) -> &'static str {
        match self {
            Path::ZeroCopy => "zero-copy",
            Path::Copy => "copy",
            Path::CopyScaleCuda => "copy+scale_cuda",
        }
    }
}

/// ddagrab frame pacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pacing {
    /// ddagrab `dup_frames`: repeat the last frame when the desktop did not change.
    pub dup_frames: bool,
    /// ddagrab `framerate` (polling rate); normally the stream fps.
    pub grab_fps: u32,
    /// Decimate to this rate with `select` on the real capture times (grab faster, keep frames
    /// >= ~1/fps apart): frames follow the desktop's own refresh instead of ddagrab's timer.
    pub select_fps: Option<u32>,
}

/// Everything needed to build one ffmpeg command line.
#[derive(Clone, Debug)]
pub struct Plan {
    pub backend: Backend,
    pub codec: Codec,
    pub cap: Capture,
    pub path: Path,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub bitrate: u64,
    /// Ceiling (plain CBR) or fixed rate (pad to `bitrate`).
    pub rate: RateMode,
    /// Software encoders only.
    pub threads: u32,
    pub preset: Option<String>,
    pub pacing: Pacing,
    pub draw_mouse: bool,
    /// NVENC copy path: upload to CUDA ourselves (`hwupload_cuda`) instead of handing NVENC
    /// system-memory frames.
    pub cuda_upload: bool,
}

impl Plan {
    pub fn encoder(&self) -> &'static str {
        self.backend.encoder(self.codec).unwrap_or("?")
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {} ({}) capture {} {}x{} on {} -> {}x{}@{} {:.1} Mb/s {}, {}",
            self.backend.name(),
            self.codec.name(),
            self.encoder(),
            self.cap.pref.name(),
            self.cap.width,
            self.cap.height,
            crate::monitors::vendor_name(self.cap.vendor),
            self.w,
            self.h,
            self.fps,
            f64_from_u64(self.bitrate) / 1e6,
            self.rate.name(),
            self.path.name()
        )
    }
}

/// Capture/transfer candidates for a backend, best first. `caps` are the working capture views of
/// the chosen monitor (power-saving view first).
pub fn candidates(b: Backend, caps: &[Capture], w: u32, h: u32) -> Vec<(Capture, Path)> {
    let mut v = Vec::new();
    if let Some(vendor) = b.vendor() {
        for c in caps.iter().filter(|c| c.vendor == vendor) {
            let same_size = c.width == w && c.height == h;
            match b {
                // QSV: VPP scales/converts on the Intel GPU and the encoder reads the same surface.
                Backend::Qsv => v.push((c.clone(), Path::ZeroCopy)),
                // NVENC/AMF read the BGRA D3D11 capture texture directly (they convert to YUV
                // themselves) but cannot scale.
                Backend::Nvenc | Backend::Amf if same_size => v.push((c.clone(), Path::ZeroCopy)),
                _ => {}
            }
        }
    }
    for c in caps {
        if b.vendor() == Some(c.vendor) && b == Backend::Qsv {
            continue; // already zero-copy
        }
        if b == Backend::Nvenc
            && (u64::from(w) * u64::from(h)) > (u64::from(c.width) * u64::from(c.height))
            && c.vendor == VENDOR_INTEL
        {
            // Upscaling: move the smaller capture-size frame, scale on the NVIDIA GPU.
            v.push((c.clone(), Path::CopyScaleCuda));
        }
        v.push((c.clone(), Path::Copy));
    }
    v
}

/// Video filter chain after the timestamping filters: capture texture -> what the encoder wants.
pub fn convert_filters(p: &Plan) -> String {
    let (w, h) = (p.w, p.h);
    let intel = p.cap.vendor == VENDOR_INTEL;
    // Intel VPP converts BGRA -> NV12. Its default matrix is BT.601 (measured: green -> Y144 Cb54
    // Cr35), so ask for BT.709 limited range (green -> Y172 Cb42 Cr27) to match the stream's VUI.
    let vpp = |w: u32, h: u32| {
        format!(
            "hwmap=derive_device=qsv,format=qsv,vpp_qsv=w={w}:h={h}:format=nv12:async_depth=0:out_range=1:\
             out_color_matrix=bt709:out_color_primaries=bt709:out_color_transfer=bt709"
        )
    };
    // CPU conversion (capture GPU is not Intel): download BGRA, swscale to the encoder's format.
    let sw = |fmt: &str| {
        format!(
            "hwdownload,format=bgra,scale=w={w}:h={h}:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,\
             format={fmt},setparams=range=tv:colorspace=bt709:color_primaries=bt709:color_trc=bt709"
        )
    };
    let planar = matches!(p.backend, Backend::X265 | Backend::SvtAv1); // want yuv420p, not nv12
    match p.path {
        Path::ZeroCopy => match p.backend {
            Backend::Qsv => vpp(w, h),
            _ => String::new(), // NVENC / AMF take the D3D11 BGRA texture as is
        },
        Path::CopyScaleCuda => {
            let (cw, ch) = (p.cap.width, p.cap.height);
            format!("{},hwdownload,format=nv12,hwupload_cuda,scale_cuda=w={w}:h={h}:interp_algo=bilinear", vpp(cw, ch))
        }
        Path::Copy => {
            let mut s = if intel {
                format!("{},hwdownload,format=nv12", vpp(w, h))
            } else {
                sw(if planar { "yuv420p" } else { "nv12" })
            };
            if intel && planar {
                s.push_str(",format=yuv420p");
            }
            if p.backend == Backend::Nvenc && p.cuda_upload {
                s.push_str(",hwupload_cuda");
            }
            s
        }
    }
}

/// SVT-AV1's buffer limits (`buf-sz`, ms of the rate; it rejects values outside them).
const SVT_BUF_MIN_MS: u64 = 20;
const SVT_BUF_MAX_MS: u64 = 10_000;

/// `bufsize` bits clamped to what SVT-AV1 accepts at `bitrate` (libsvtav1 sets `buf-sz` = bufsize / bitrate
/// in ms, so one 60 fps frame, 16.7 ms, is too small).
fn svt_bufsize(bufsize: u64, bitrate: u64) -> u64 {
    let lo = (bitrate * SVT_BUF_MIN_MS).div_ceil(1000);
    let hi = bitrate.saturating_mul(SVT_BUF_MAX_MS) / 1000;
    bufsize.clamp(lo.min(hi), hi.max(lo))
}

/// `-b:v`/`-maxrate`/`-bufsize` for SVT-AV1: the 1-frame VBV, clamped to SVT's buffer range (logged).
fn svtav1_rate(v: &mut Vec<String>, p: &Plan, b: &str, bitrate: u64) {
    let vbv = (p.bitrate / u64::from(p.fps.max(1))).max(1);
    let svt = svt_bufsize(vbv, bitrate);
    if svt != vbv {
        eprintln!(
            "[encoder] svtav1: VBV {vbv} bits is outside SVT-AV1's 20..10000 ms; using {svt} bits (SVT accepts {SVT_BUF_MIN_MS}..{SVT_BUF_MAX_MS} ms)"
        );
    }
    v.extend(["-b:v", b, "-maxrate", b, "-bufsize", &svt.to_string()].map(String::from));
}

/// Encoder options (after `-c:v <name>`), all tuned for latency.
pub fn encoder_args(p: &Plan) -> Vec<String> {
    // Rate control budgets bits per frame from the stream's nominal frame rate. The `select`
    // pacing modes keep ddagrab's nominal rate (grab_fps, e.g. 240) while only ~fps frames are
    // sent, so scale the rate up by grab_fps / fps to get `bitrate` on the wire (measured on the
    // TV before this fix: 7.3 Mb/s instead of 30 at 4x grab rate).
    let scale = match p.pacing.select_fps {
        Some(f) if f > 0 => f64::from(p.pacing.grab_fps.max(f)) / f64::from(f),
        _ => 1.0,
    };
    let bitrate = u64_sat_from_f64(f64_from_u64(p.bitrate) * scale);
    let b = bitrate.to_string();
    // VBV of ~1 frame at the real frame rate.
    let bufsize = (p.bitrate / u64::from(p.fps.max(1))).max(1).to_string();
    let rate = |v: &mut Vec<String>| v.extend(["-b:v", &b, "-maxrate", &b, "-bufsize", &bufsize].map(String::from));
    let mut v: Vec<String> = vec!["-c:v".into(), p.encoder().into()];
    let s = |v: &mut Vec<String>, a: &[&str]| v.extend(a.iter().map(std::string::ToString::to_string));
    let fixed = p.rate == RateMode::Fixed;
    match p.backend {
        Backend::Nvenc => {
            let preset = p.preset.clone().unwrap_or_else(|| "p1".into());
            s(&mut v, &["-preset", &preset, "-tune", "ull", "-zerolatency", "1", "-delay", "0", "-rc", "cbr"]);
            rate(&mut v);
            if fixed {
                // Quality first (min QP), then pad up to the target (NVENC filler data, H.264 and HEVC).
                s(&mut v, &["-qmin", &FIXED_MIN_QP.to_string(), "-cbr_padding", "1"]);
            }
            s(&mut v, &["-bf", "0", "-g", "600", "-forced-idr", "1"]);
            match p.codec {
                Codec::H264 => {
                    s(&mut v, &["-profile:v", "high"]);
                    // A level that carries the real stream (size, fps, bitrate): NVENC's auto level stops at
                    // what the size and frame rate need. Validated before planning (`h264_level_for`).
                    if let Ok(idc) = level::h264_level_for(p.w, p.h, p.fps, p.bitrate) {
                        s(&mut v, &["-level", &format!("{}.{}", idc / 10, idc % 10)]);
                    }
                }
                Codec::Hevc => s(&mut v, &["-profile:v", "main"]),
                Codec::Av1 => {}
            }
        }
        Backend::Qsv => {
            let preset = p.preset.clone().unwrap_or_else(|| "veryfast".into());
            s(&mut v, &["-preset", &preset, "-low_power", "1", "-async_depth", "1", "-low_delay_brc", "1"]);
            rate(&mut v);
            s(&mut v, &["-bf", "0", "-g", "600", "-forced_idr", "1"]);
            if p.codec == Codec::H264 {
                s(&mut v, &["-look_ahead", "0", "-profile:v", "high"]);
            }
        }
        Backend::Amf => {
            s(
                &mut v,
                &[
                    "-usage",
                    "ultralowlatency",
                    "-quality",
                    "speed",
                    "-rc",
                    "cbr",
                    "-async_depth",
                    "1",
                    "-forced_idr",
                    "1",
                ],
            );
            rate(&mut v);
            s(&mut v, &["-g", "600", "-filler_data", if fixed { "1" } else { "0" }, "-preanalysis", "0"]);
            match p.codec {
                Codec::H264 => s(&mut v, &["-bf", "0", "-latency", "1", "-enforce_hrd", "1", "-vbaq", "0"]),
                Codec::Hevc => s(&mut v, &["-latency", "1", "-header_insertion_mode", "idr", "-enforce_hrd", "1"]),
                // av1_amf names its key-frame-aligned mode `gop` (none|gop|frame); `idr` is HEVC's name.
                Codec::Av1 => s(&mut v, &["-bf", "0", "-latency", "lowest_latency", "-header_insertion_mode", "gop"]),
            }
        }
        Backend::Mf => {
            // display_remoting: the MFT's screen-content low-latency mode; low_delay sets
            // CODECAPI_AVLowLatencyMode.
            s(
                &mut v,
                &["-hw_encoding", "1", "-scenario", "display_remoting", "-rate_control", "cbr", "-flags", "+low_delay"],
            );
            rate(&mut v);
            s(&mut v, &["-bf", "0", "-g", "600"]);
        }
        Backend::X264 => {
            let preset = p.preset.clone().unwrap_or_else(|| "ultrafast".into());
            s(&mut v, &["-preset", &preset, "-tune", "zerolatency", "-threads", &p.threads.to_string()]);
            rate(&mut v);
            if fixed {
                s(&mut v, &["-nal-hrd", "cbr"]); // hard CBR: x264 writes filler
            }
            s(&mut v, &["-bf", "0", "-g", "600", "-forced-idr", "1"]);
        }
        Backend::X265 => {
            let preset = p.preset.clone().unwrap_or_else(|| "ultrafast".into());
            s(&mut v, &["-preset", &preset, "-tune", "zerolatency", "-forced-idr", "1"]);
            rate(&mut v);
            let xp =
                format!("bframes=0:keyint=600:pools={}:frame-threads=1:repeat-headers=1:log-level=error", p.threads);
            s(&mut v, &["-x265-params", &xp]);
        }
        Backend::SvtAv1 => {
            // Real-time mode (rtc), low-delay prediction structure, CBR.
            let preset = p.preset.clone().unwrap_or_else(|| "12".into());
            s(&mut v, &["-preset", &preset, "-g", "600"]);
            svtav1_rate(&mut v, p, &b, bitrate);
            let sp = format!("rtc=1:pred-struct=1:rc=2:lp={}", p.threads);
            s(&mut v, &["-svtav1-params", &sp]);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(vendor: u32, w: u32, h: u32) -> Capture {
        Capture { pref: GpuPref::PowerSaving, output_idx: 0, vendor, adapter: "x".into(), width: w, height: h }
    }

    #[test]
    fn order() {
        assert_eq!(
            auto_order(Codec::H264),
            vec![Backend::Nvenc, Backend::Qsv, Backend::Amf, Backend::Mf, Backend::X264]
        );
        assert_eq!(auto_order(Codec::Av1), vec![Backend::Nvenc, Backend::Amf, Backend::SvtAv1]);
        assert_eq!(auto_order(Codec::Hevc).last(), Some(&Backend::X265));
    }

    #[test]
    fn bitrates() {
        assert_eq!(default_bitrate(Codec::H264, 1920, 1080, 60), 30_000_000);
        assert_eq!(default_bitrate(Codec::H264, 3840, 2160, 60), 60_000_000);
        assert_eq!(default_bitrate(Codec::Hevc, 3840, 2160, 60), 100_000_000);
        assert_eq!(default_bitrate(Codec::Av1, 3840, 2160, 60), 100_000_000);
        assert_eq!(default_bitrate(Codec::Hevc, 1920, 1080, 60), 20_000_000);
    }

    fn plan(backend: Backend, codec: Codec, bitrate: u64, rate: RateMode) -> Plan {
        Plan {
            backend,
            codec,
            cap: cap(VENDOR_NVIDIA, 3840, 2160),
            path: Path::ZeroCopy,
            w: 3840,
            h: 2160,
            fps: 60,
            bitrate,
            rate,
            threads: 4,
            preset: None,
            pacing: Pacing { dup_frames: true, grab_fps: 60, select_fps: None },
            draw_mouse: true,
            cuda_upload: true,
        }
    }

    /// The value after `flag` in an argument list.
    fn arg<'a>(v: &'a [String], flag: &str) -> Option<&'a str> {
        v.iter().position(|a| a == flag).and_then(|i| v.get(i + 1)).map(String::as_str)
    }

    #[test]
    fn nvenc_rate_args_reach_the_encoder() {
        for rate in [RateMode::Ceiling, RateMode::Fixed] {
            let v = encoder_args(&plan(Backend::Nvenc, Codec::Hevc, 150_000_000, rate));
            assert_eq!(arg(&v, "-rc"), Some("cbr"));
            assert_eq!(arg(&v, "-b:v"), Some("150000000"));
            assert_eq!(arg(&v, "-maxrate"), Some("150000000"));
            assert_eq!(arg(&v, "-bufsize"), Some("2500000"), "1-frame VBV");
            assert_eq!(arg(&v, "-bf"), Some("0"));
            let fixed = rate == RateMode::Fixed;
            assert_eq!(arg(&v, "-cbr_padding"), fixed.then_some("1"), "{rate:?}");
            assert_eq!(arg(&v, "-qmin"), fixed.then_some("1"), "{rate:?}");
        }
    }

    #[test]
    fn fixed_rate_pads_where_the_encoder_can() {
        let amf = |r| encoder_args(&plan(Backend::Amf, Codec::Hevc, 100_000_000, r));
        assert_eq!(arg(&amf(RateMode::Fixed), "-filler_data"), Some("1"));
        assert_eq!(arg(&amf(RateMode::Ceiling), "-filler_data"), Some("0"));
        let x264 = |r| encoder_args(&plan(Backend::X264, Codec::H264, 30_000_000, r));
        assert_eq!(arg(&x264(RateMode::Fixed), "-nal-hrd"), Some("cbr"));
        assert_eq!(arg(&x264(RateMode::Ceiling), "-nal-hrd"), None);
        // No padding option: the arguments do not change.
        let qsv = |r| encoder_args(&plan(Backend::Qsv, Codec::Hevc, 100_000_000, r));
        assert_eq!(qsv(RateMode::Fixed), qsv(RateMode::Ceiling));
    }

    #[test]
    fn amf_header_insertion_mode_per_codec() {
        // ffmpeg 8.1: hevc_amf takes none|gop|idr, av1_amf takes none|gop|frame ("idr" fails to open).
        let amf = |c| encoder_args(&plan(Backend::Amf, c, 30_000_000, RateMode::Ceiling));
        assert_eq!(arg(&amf(Codec::Hevc), "-header_insertion_mode"), Some("idr"));
        assert_eq!(arg(&amf(Codec::Av1), "-header_insertion_mode"), Some("gop"));
        assert_eq!(arg(&amf(Codec::H264), "-header_insertion_mode"), None);
    }

    #[test]
    fn fixed_only_where_the_backend_pads() {
        use RateMode::{Ceiling, Fixed};
        assert_eq!(effective_rate(Fixed, Backend::Nvenc, NvencPadding::Supported), Fixed);
        assert_eq!(
            effective_rate(Fixed, Backend::Nvenc, NvencPadding::Missing),
            Ceiling,
            "FFmpeg 7.x: no -cbr_padding"
        );
        assert_eq!(effective_rate(Fixed, Backend::Amf, NvencPadding::Missing), Fixed);
        assert_eq!(effective_rate(Fixed, Backend::X264, NvencPadding::Missing), Fixed);
        for b in [Backend::Qsv, Backend::Mf, Backend::X265, Backend::SvtAv1] {
            assert_eq!(effective_rate(Fixed, b, NvencPadding::Supported), Ceiling, "{b:?} cannot pad");
        }
        assert_eq!(effective_rate(Ceiling, Backend::Nvenc, NvencPadding::Supported), Ceiling);
        // A ceiling plan never asks ffmpeg for -cbr_padding, so an old ffmpeg starts.
        let v = encoder_args(&plan(
            Backend::Nvenc,
            Codec::Hevc,
            100_000_000,
            effective_rate(Fixed, Backend::Nvenc, NvencPadding::Missing),
        ));
        assert_eq!(arg(&v, "-cbr_padding"), None);
    }

    #[test]
    fn the_asked_bitrate_passes_through_for_every_codec() {
        for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
            assert_eq!(target_bitrate(Some(150_000_000), c, 3840, 2160, 60), 150_000_000, "{c:?}");
            assert_eq!(target_bitrate(Some(40_000_000), c, 3840, 2160, 60), 40_000_000, "{c:?}");
        }
        assert_eq!(target_bitrate(None, Codec::H264, 3840, 2160, 60), 60_000_000);
        assert_eq!(target_bitrate(None, Codec::Hevc, 3840, 2160, 60), 100_000_000);
    }

    #[test]
    fn svtav1_buffer_stays_in_its_20_to_10000_ms_range() {
        for (b, fps) in [(25_000_000, 60), (75_000_000, 60), (150_000_000, 60), (25_000_000, 30)] {
            let mut p = plan(Backend::SvtAv1, Codec::Av1, b, RateMode::Ceiling);
            p.fps = fps;
            let v = encoder_args(&p);
            let buf: u64 = arg(&v, "-bufsize").expect("bufsize").parse().expect("number");
            let ms = buf * 1000 / b;
            assert!((20..=10_000).contains(&ms), "{b} @ {fps}: {buf} bits = {ms} ms");
            assert_eq!(arg(&v, "-b:v"), Some(b.to_string().as_str()));
        }
        assert_eq!(svt_bufsize(1, 1_000_000), 20_000, "raised to 20 ms");
        assert_eq!(svt_bufsize(u64::MAX, 1_000_000), 10_000_000, "lowered to 10 s");
        assert_eq!(svt_bufsize(50_000, 1_000_000), 50_000, "inside the range: unchanged");
        // The other encoders keep the 1-frame VBV.
        let x = encoder_args(&plan(Backend::X265, Codec::Hevc, 60_000_000, RateMode::Ceiling));
        assert_eq!(arg(&x, "-bufsize"), Some("1000000"));
    }

    #[test]
    fn nvenc_h264_level_carries_the_bitrate() {
        let lvl =
            |b| arg(&encoder_args(&plan(Backend::Nvenc, Codec::H264, b, RateMode::Fixed)), "-level").map(String::from);
        assert_eq!(lvl(150_000_000).as_deref(), Some("5.2"), "4K60 at 150 Mb/s");
        assert_eq!(lvl(300_000_000).as_deref(), Some("6.1"));
        let v = encoder_args(&plan(Backend::Nvenc, Codec::H264, 150_000_000, RateMode::Fixed));
        assert_eq!(arg(&v, "-b:v"), Some("150000000"), "no cap");
        let hevc = encoder_args(&plan(Backend::Nvenc, Codec::Hevc, 150_000_000, RateMode::Fixed));
        assert_eq!(arg(&hevc, "-level"), None, "HEVC keeps NVENC's own level");
    }

    #[test]
    fn select_pacing_scales_the_rate_but_keeps_the_frame_budget() {
        let mut p = plan(Backend::Nvenc, Codec::Hevc, 100_000_000, RateMode::Fixed);
        p.pacing = Pacing { dup_frames: false, grab_fps: 240, select_fps: Some(60) };
        let v = encoder_args(&p);
        assert_eq!(arg(&v, "-b:v"), Some("400000000"));
        assert_eq!(arg(&v, "-bufsize"), Some("1666666"));
    }

    #[test]
    fn hybrid_laptop_candidates() {
        // Intel owns the output: QSV zero-copy; NVENC copies (and scales on CUDA when upscaling).
        let caps = [cap(VENDOR_INTEL, 1920, 1080)];
        assert_eq!(candidates(Backend::Qsv, &caps, 1920, 1080), vec![(caps[0].clone(), Path::ZeroCopy)]);
        assert_eq!(candidates(Backend::Nvenc, &caps, 1920, 1080), vec![(caps[0].clone(), Path::Copy)]);
        assert_eq!(
            candidates(Backend::Nvenc, &caps, 3840, 2160),
            vec![(caps[0].clone(), Path::CopyScaleCuda), (caps[0].clone(), Path::Copy)]
        );
        // NVIDIA desktop: NVENC reads the capture texture when no scaling is needed.
        let caps = [cap(VENDOR_NVIDIA, 1920, 1080)];
        assert_eq!(candidates(Backend::Nvenc, &caps, 1920, 1080)[0].1, Path::ZeroCopy);
        assert_eq!(candidates(Backend::Nvenc, &caps, 1280, 720), vec![(caps[0].clone(), Path::Copy)]);
    }
}
