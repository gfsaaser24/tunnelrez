//! `--backend native`: in-process capture (DDA / WGC) -> GPU colour conversion -> NVENC SDK, from the
//! `tunnelrez_native` crate (`../native`, cargo feature `native`, on by default).
//!
//! [`start`] wraps a `NativeSession` in a [`Source`], so the session code (and the bytes on the wire)
//! are the same as for the ffmpeg backend. Each access unit is handed on the moment it is ready.

pub use crate::encoder::RateMode;
use crate::flv::Codec;
#[cfg(not(feature = "native"))]
use crate::source::Source;

/// Options only the native backend uses.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeOpts {
    /// NVENC preset 1..=7 (P1 fastest).
    pub preset: u8,
    /// `auto`, `wgc` or `dda`.
    pub capture: String,
    /// Intra refresh period in frames (0 = IDR on request only).
    pub intra_refresh: u32,
    pub sync_encode: bool,
    /// VBV size in frames.
    pub vbv_frames: f32,
    /// NVENC split-frame mode: 0 auto, 1 forced, 2 two strips, 15 off.
    pub split: u32,
}

impl Default for NativeOpts {
    fn default() -> Self {
        NativeOpts {
            preset: 1,
            capture: "auto".into(),
            intra_refresh: 0,
            sync_encode: false,
            vbv_frames: 1.0,
            split: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeCfg {
    /// GDI device name, e.g. `\\.\DISPLAY10`.
    pub output: String,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    /// Ceiling (plain CBR) or fixed rate (NVENC filler up to `bitrate_bps`).
    pub rate: RateMode,
    /// H.264 `level_idc` that carries the stream (52 = 5.2; see `level::h264_level_for`); None = NVENC
    /// picks (HEVC).
    pub h264_level: Option<u8>,
    pub opts: NativeOpts,
}

/// True when this build has the native backend.
pub fn available() -> bool {
    cfg!(feature = "native")
}

/// The native backend encodes this codec (NVENC SDK path: HEVC, H.264 and AV1; AV1 needs an RTX 40 or
/// newer, which the session checks when it starts).
pub fn supports(c: Codec) -> bool {
    matches!(c, Codec::H264 | Codec::Hevc | Codec::Av1)
}

/// Start a native session for `c`.
///
/// # Errors
/// When this build has no native backend, the codec is AV1, or the session cannot start
/// (no NVIDIA GPU, driver older than 610, output not found).
#[cfg(not(feature = "native"))]
pub fn start(_c: &NativeCfg) -> anyhow::Result<Box<dyn Source>> {
    anyhow::bail!("--backend native is not in this build (built without the `native` feature); use --backend ffmpeg")
}

#[cfg(feature = "native")]
pub use imp::start;

#[cfg(feature = "native")]
mod imp {
    use super::{supports, NativeCfg, RateMode};
    use crate::flv::Codec;
    use crate::source::{Au, Event, Source, Stages, StopMode};
    use anyhow::{bail, Context, Result};
    use tunnelrez_native as sn;

    impl From<RateMode> for sn::RateMode {
        fn from(r: RateMode) -> Self {
            match r {
                RateMode::Ceiling => sn::RateMode::Ceiling,
                RateMode::Fixed => sn::RateMode::Fixed,
            }
        }
    }

    /// How long one `next` call waits for an access unit before it returns [`Event::Idle`].
    const POLL_MS: u32 = 100;

    struct NativeSource {
        s: sn::NativeSession,
        config: Option<Vec<u8>>,
        last_error: String,
    }

    fn config(c: &NativeCfg) -> Result<sn::NativeConfig> {
        if !supports(c.codec) {
            bail!("native backend: {} is not supported (HEVC, H.264 or AV1)", c.codec.name());
        }
        Ok(sn::NativeConfig {
            output: sn::OutputSel::Name(c.output.clone()),
            codec: match c.codec {
                Codec::Hevc => sn::Codec::Hevc,
                Codec::H264 => sn::Codec::H264,
                Codec::Av1 => sn::Codec::Av1,
            },
            width: c.width,
            height: c.height,
            fps: c.fps,
            bitrate_bps: u32::try_from(c.bitrate_bps).ok().with_context(|| {
                format!(
                    "native backend: {:.1} Mb/s is above NVENC's limit ({:.1} Mb/s); use a lower --bitrate",
                    crate::num::f64_from_u64(c.bitrate_bps) / 1e6,
                    f64::from(u32::MAX) / 1e6
                )
            })?,
            h264_level: c.h264_level.map(u32::from),
            capture: match c.opts.capture.as_str() {
                "wgc" => sn::CaptureApi::Wgc,
                "dda" => sn::CaptureApi::Dda,
                _ => sn::CaptureApi::Auto,
            },
            preset: c.opts.preset.clamp(1, 7),
            intra_refresh_frames: c.opts.intra_refresh,
            sync_encode: c.opts.sync_encode,
            vbv_frames: c.opts.vbv_frames,
            split_mode: c.opts.split,
            rate_mode: c.rate.into(),
            ..Default::default()
        })
    }

    /// Start a native session for `c`.
    ///
    /// # Errors
    ///
    /// When the codec is AV1, or the session cannot start (no NVIDIA GPU, driver older than
    /// 610, output not found).
    pub fn start(c: &NativeCfg) -> Result<Box<dyn Source>> {
        let cfg = config(c)?;
        sn::init_timer();
        let s = sn::NativeSession::start(cfg.clone())?;
        let i = s.info();
        eprintln!(
            "[native] {} {:?} on {} -> {:?} -> NVENC {} on {} {}x{} -> {}x{} P{} {} split={} engines={} {:.1} Mb/s {}",
            i.output,
            i.capture,
            i.capture_gpu,
            i.copy,
            c.codec.name(),
            i.encode_gpu,
            i.source_size.0,
            i.source_size.1,
            i.encode_size.0,
            i.encode_size.1,
            cfg.preset,
            if i.async_encode { "async" } else { "sync" },
            cfg.split_mode,
            i.nvenc_engines,
            f64::from(cfg.bitrate_bps) / 1e6,
            c.rate.name(),
        );
        let config = s.config_bytes();
        Ok(Box::new(NativeSource { s, config: Some(config), last_error: String::new() }))
    }

    impl Source for NativeSource {
        fn next(&mut self) -> Result<Option<Event>> {
            if let Some(cf) = self.config.take().filter(|c| !c.is_empty()) {
                return Ok(Some(Event::Config(cf)));
            }
            match self.s.poll_au(POLL_MS) {
                Ok(Some(au)) => Ok(Some(Event::Frame(Au {
                    data: au.data,
                    cap_us: au.capture_t_us,
                    arrive_us: au.ready_t_us,
                    key: au.keyframe,
                    stages: Some(Stages {
                        arrive_us: au.arrive_t_us,
                        convert_done_us: au.convert_done_t_us,
                        xfer_us: au.xfer_us,
                        encode_start_us: au.encode_start_t_us,
                        repeat: au.repeat,
                    }),
                }))),
                Ok(None) => Ok(Some(Event::Idle)),
                Err(e) => {
                    self.last_error = format!("{e:#}");
                    Err(e)
                }
            }
        }

        fn force_idr(&mut self) -> bool {
            self.s.force_idr();
            true
        }

        fn exit_reason(&mut self) -> String {
            format!("native session: {}", self.last_error)
        }

        fn stop(self: Box<Self>, _mode: StopMode) {}
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn cfg(bitrate_bps: u64, rate: RateMode) -> NativeCfg {
            NativeCfg {
                output: r"\\.\DISPLAY10".into(),
                codec: Codec::Hevc,
                width: 3840,
                height: 2160,
                fps: 60,
                bitrate_bps,
                rate,
                h264_level: None,
                opts: super::super::NativeOpts::default(),
            }
        }

        #[test]
        fn bitrate_and_rate_mode_reach_nvenc() {
            let n = config(&cfg(150_000_000, RateMode::Fixed)).expect("hevc is supported");
            assert_eq!(n.bitrate_bps, 150_000_000);
            assert_eq!(n.rate_mode, sn::RateMode::Fixed);
            assert_eq!((n.width, n.height, n.fps), (3840, 2160, 60));
            let n = config(&cfg(100_000_000, RateMode::Ceiling)).expect("hevc is supported");
            assert_eq!(n.rate_mode, sn::RateMode::Ceiling);
            // Out of NVENC's u32 range: a clear error, never a silent clamp.
            let e = config(&cfg(5_000_000_000, RateMode::Fixed)).expect_err("too high").to_string();
            assert!(e.contains("5000.0 Mb/s"), "{e}");
            let mut h = cfg(150_000_000, RateMode::Fixed);
            (h.codec, h.h264_level) = (Codec::H264, Some(52));
            let n = config(&h).expect("h264 is supported");
            assert_eq!((n.bitrate_bps, n.h264_level), (150_000_000, Some(52)));
        }

        #[test]
        fn av1_reaches_nvenc() {
            let mut c = cfg(100_000_000, RateMode::Fixed);
            c.codec = Codec::Av1;
            let n = config(&c).expect("av1 is supported");
            assert_eq!((n.codec, n.bitrate_bps), (sn::Codec::Av1, 100_000_000));
            assert!(supports(Codec::Av1));
        }
    }
}
