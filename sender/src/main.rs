//! tunnelrez-send: PC side of `TunnelRez` (protocol v0, see ../PROTOCOL.md).
//!
//! Backends:
//!   * `--backend ffmpeg` (default): ffmpeg `ddagrab` (D3D11 desktop duplication) -> GPU
//!     convert/scale -> NVENC / `QuickSync` / AMF / Media Foundation / x264 / x265 / SVT-AV1,
//!     picked by `--encoder auto` (probe, cache, demote on failure) -> FLV on stdout -> TCP.
//!   * `--backend native`: zero-copy D3D11 -> NVENC SDK (crate `tunnelrez_native`, feature `native`).
//!
//! Both feed the same session code, so the TV sees the same stream.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::net::ToSocketAddrs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};
use tunnelrez::audio::sink::SinkChoice;
use tunnelrez::audio::stream::AudioMode;
use tunnelrez::encoder::{self, Backend, Capture, GpuPref, NvencPadding, Pacing, Path, Plan, RateMode};
use tunnelrez::ffmpeg::{FfmpegSource, RunOpts};
use tunnelrez::flv::Codec;
use tunnelrez::level;
use tunnelrez::monitors::VENDOR_NVIDIA;
use tunnelrez::monitors::{self, Monitor};
use tunnelrez::native::{self, NativeCfg, NativeOpts};
use tunnelrez::num::{self, u64_sat_from_f64};
use tunnelrez::probe::{self, Verbosity};
use tunnelrez::session::{self, End, STOP};
use tunnelrez::source::{self, Source};
use tunnelrez::tools::{Display, Tools, Views};
use tunnelrez::winutil;

const USAGE: &str = "\
tunnelrez-send --tv <ip> [--port 7878] [--monitor N | --extend] [--codec h264|hevc|av1]
                [--backend ffmpeg|native] [--encoder auto|nvenc|qsv|amf|mf|x264|x265|svtav1]
                [--size WxH|native] [--fps 60] [--bitrate 30M] [--threads N] [--preset P]
                [--pacing dup|smooth|new] [--duration SECS] [--ffmpeg PATH] [--reprobe] [--verbose]
                [--capture auto|wgc|dda] [--intra-refresh N] [--sync-encode] [--vbv FRAMES]
                [--split auto|forced|two|off] [--fixed-rate on|off] [--audio on|off|tv-only]
                [--audio-sink auto|off|NAME]
tunnelrez-send doctor [--codec C] [--monitor N | --extend] [--size WxH|native] [--fps 60]
tunnelrez-send caps [--monitor N | --extend] [--size WxH|native] [--fps 60] [--reprobe]
tunnelrez-send --list-monitors

  --list-monitors   list capturable monitors and exit
  --monitor N       monitor index from --list-monitors (default: the primary monitor)
  --extend          capture the Parsec virtual display (plug one with --size/--fps if none exists)
  --backend         ffmpeg or native (in-process capture -> NVENC SDK, H.264/HEVC, lowest latency).
                    Not given: decided by --encoder.
  --encoder         auto (default): the native NVENC backend when this build has it, the codec is
                    H.264/HEVC and an NVIDIA GPU renders the monitor; else ffmpeg: probe nvenc -> qsv
                    -> amf -> mf -> software, cache the first that works, move to the next after 3
                    failures in a row (native falls back to ffmpeg the same way). Or name one ffmpeg
                    encoder.
  --size            output size (default 1920x1080); `native` = capture size, no scaling
  --bitrate         default per codec and size: 1080p60 h264 30M / hevc 20M / av1 16M,
                    4K60 h264 60M / hevc 100M / av1 100M. A given --bitrate is used as is for
                    every codec (H.264 gets the level that carries it: 4K60 up to 240M is 5.2)
  --fixed-rate      on: the stream really sends --bitrate (quality first via a low min QP, then
                    filler data pads every frame; NVENC, native, AMF, x264). Every frame is then full
                    size, so it takes longer on the network (150M: ~2.5 ms/frame on gigabit, ~8 ms
                    on 300 Mb/s Wi-Fi). off: --bitrate is a ceiling, simple content sends less.
                    Default: on when --bitrate is given, else off.
  --threads         software encoder threads (default: half the CPUs, 2..8)
  --preset          encoder preset override (nvenc p1-p7, qsv veryfast.., x264 ultrafast.., svtav1 0-13;
                    native: 1-7 or p1-p7, default 1)
  --fps             stream rate, 60 max (the virtual monitor always runs at 60 Hz)
  native only:      --capture (auto = WGC, else DDA), --intra-refresh (frames, 0 = IDR on request
                    only), --sync-encode (NVENC sync mode), --vbv FRAMES (default 1),
                    --split auto|forced|two|off (HEVC split-frame encode, default auto)
  --pacing          dup (default): ddagrab repeats unchanged frames on its own timer, constant fps.
                    smooth: ddagrab polls at 4x fps and frames >= ~1/fps apart are kept, so they
                    follow the desktop refresh. new: like smooth but only changed frames (nothing
                    sent while the desktop is still). qsv always uses dup. Measured on the TV
                    (1080p60 NVENC, moving content): smooth was not smoother than dup and added
                    ~5 ms; new added ~50 ms and dropped to ~56 fps. Keep dup.
  --reprobe         ignore the cached encoder choice
  doctor            probe every backend for the codec (all codecs if --codec is not given) and print
                    works/fails, why, and the measured capture->encoded ms, fps and CPU
  caps              one JSON line: for each encoder (native, nvenc, qsv, amf, mf, cpu) and codec
                    (h264, hevc, av1), whether it really runs at --size/--fps on this PC (a short
                    real encode that must keep up; cached, --reprobe tests again) and why not
  --audio           on (default): the PC's sound plays on the TV too (WASAPI loopback, raw PCM over UDP,
                    synced to the picture; only with a TV app that has audio). off: video only.
                    tv-only: the PC itself stays silent while streaming
  --audio-sink      auto (default): if a virtual audio device is present (Steam Streaming Speakers,
                    VB-CABLE 'CABLE Input', Virtual Audio Driver, TunnelRez TV), make it the Windows default
                    playback device while streaming and capture it; the old defaults come back after.
                    Then --audio on also plays the sound on the PC speakers ('Also play on PC'), and
                    tv-only needs no mute. NAME: that device (endpoint ID or name). off: never; capture
                    the PC speakers, and tv-only mutes them (restored after)
  audio-probe       check that loopback capture survives a muted PC (silent; mute/volume restored)
  audio-restore     put back a default playback device or mute a killed sender left behind
  audio-devices     one JSON line: the playback devices, and the virtual sink auto would use
  audio-sink        status (one JSON line) | install [--dry-run] | uninstall [--force]: the TV speaker
                    (the Steam Streaming Speakers driver, bundled in drivers/steam-streaming-speakers; one
                    UAC prompt). uninstall removes only a device TunnelRez installed, unless --force.
                    restore-defaults: put back the default playback devices an install moved (only that)
  vdd               status (one JSON line) | unplug | repair [--dry-run] [--restart-adapter] | install [--dry-run] [--force]:
                    the TV monitor (the Parsec Virtual Display Driver, installer bundled in drivers/parsec-vdd). unplug
                    removes every virtual monitor (no UAC prompt). repair
                    fixes only what is wrong (one UAC prompt, none when only a stuck plug is cleared); install runs
                    the installer silently unless the driver is there; --force runs it anyway
  --duration        stop after SECS seconds (testing)
";

/// The one-shot commands. `main` runs the first one set, in this order; none set = stream.
#[derive(Clone, Copy, Debug, Default)]
#[expect(clippy::struct_excessive_bools, reason = "independent one-shot command flags, checked in order")]
struct Commands {
    /// `--probe-dxgi`: print this process's DXGI view (run by the probe helpers).
    probe_dxgi: bool,
    /// `--list-monitors`.
    list: bool,
    /// `doctor`.
    doctor: bool,
    /// `caps`.
    caps: bool,
}

/// Whether the cached encoder choice may be used (`--reprobe` ignores it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProbeCache {
    Use,
    Ignore,
}

#[derive(Clone, Debug)]
struct Args {
    tv: Option<String>,
    port: u16,
    monitor: Option<usize>,
    extend: bool,
    /// `--codec`; None = not given (H.264 for streaming, every codec for `doctor`).
    codec_arg: Option<Codec>,
    /// None: decided by `--encoder` (auto -> native when possible).
    backend_native: Option<bool>,
    native: NativeOpts,
    encoder: Option<Backend>, // None = auto
    bitrate: Option<u64>,
    /// `--fixed-rate`; None = on exactly when `--bitrate` is given.
    fixed_rate: Option<RateMode>,
    fps: u32,
    size: Option<(u32, u32)>, // None = native
    threads: Option<u32>,
    preset: Option<String>,
    pacing: String,
    duration: Option<f64>,
    ffmpeg: Option<PathBuf>,
    verbosity: Verbosity,
    cmd: Commands,
    cache: ProbeCache,
    cuda_upload: bool,
    grab_fps: Option<u32>,
    no_mouse: bool,
    audio: AudioMode,
    audio_sink: SinkChoice,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            tv: None,
            port: 7878,
            monitor: None,
            extend: false,
            codec_arg: None,
            backend_native: None,
            native: NativeOpts::default(),
            encoder: None,
            bitrate: None,
            fixed_rate: None,
            fps: 60,
            size: Some((1920, 1080)),
            threads: None,
            preset: None,
            pacing: "dup".into(),
            duration: None,
            ffmpeg: None,
            verbosity: Verbosity::Quiet,
            cmd: Commands::default(),
            cache: ProbeCache::Use,
            cuda_upload: true,
            grab_fps: None,
            no_mouse: false,
            audio: AudioMode::On,
            audio_sink: SinkChoice::Auto,
        }
    }
}

impl Args {
    /// The codec to stream (`--codec`, default H.264).
    fn codec(&self) -> Codec {
        self.codec_arg.unwrap_or(Codec::H264)
    }

    /// `--fixed-rate`, else fixed exactly when the user set `--bitrate`.
    fn rate_mode(&self) -> RateMode {
        self.fixed_rate.unwrap_or(if self.bitrate.is_some() { RateMode::Fixed } else { RateMode::Ceiling })
    }
}

fn parse_rate(s: &str) -> Result<u64> {
    let s = s.trim();
    let (n, mul) = match s.chars().last() {
        Some('M' | 'm') => (&s[..s.len() - 1], 1_000_000.0),
        Some('K' | 'k') => (&s[..s.len() - 1], 1_000.0),
        _ => (s, 1.0),
    };
    Ok(u64_sat_from_f64(n.parse::<f64>().with_context(|| format!("bad bitrate {s}"))? * mul))
}

fn parse_args() -> Result<Args> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or_else(|| anyhow!("{k} needs a value"));
        match k.as_str() {
            "--tv" => a.tv = Some(val()?),
            "--port" => a.port = val()?.parse()?,
            "--monitor" => a.monitor = Some(val()?.parse()?),
            "--list-monitors" => a.cmd.list = true,
            "--extend" => a.extend = true,
            "--codec" => {
                a.codec_arg = Some(Codec::parse(&val()?).ok_or_else(|| anyhow!("codec: h264|hevc|av1"))?);
            }
            "--backend" => {
                a.backend_native = match val()?.as_str() {
                    "ffmpeg" => Some(false),
                    "native" => Some(true),
                    o => bail!("backend {o}: ffmpeg|native"),
                }
            }
            "--encoder" => {
                let v = val()?;
                a.encoder = if v == "auto" {
                    None
                } else {
                    Some(
                        Backend::parse(&v)
                            .ok_or_else(|| anyhow!("encoder {v}: auto|nvenc|qsv|amf|mf|x264|x265|svtav1"))?,
                    )
                }
            }
            "--bitrate" => a.bitrate = Some(parse_rate(&val()?)?),
            "--fixed-rate" => {
                a.fixed_rate = Some(match val()?.as_str() {
                    "on" => RateMode::Fixed,
                    "off" => RateMode::Ceiling,
                    o => bail!("fixed-rate {o}: on|off"),
                });
            }
            "--fps" => a.fps = val()?.parse()?,
            "--size" => {
                let v = val()?;
                a.size = if v == "native" {
                    None
                } else {
                    let (w, h) = v.split_once(['x', 'X']).ok_or_else(|| anyhow!("size WxH or native"))?;
                    Some((w.parse()?, h.parse()?))
                }
            }
            "--threads" => a.threads = Some(val()?.parse()?),
            "--preset" => a.preset = Some(val()?),
            "--pacing" => {
                a.pacing = val()?;
                if !["dup", "new", "smooth"].contains(&a.pacing.as_str()) {
                    bail!("pacing {}: dup|new|smooth", a.pacing);
                }
            }
            "--capture" => {
                a.native.capture = val()?;
                if !["auto", "wgc", "dda"].contains(&a.native.capture.as_str()) {
                    bail!("capture {}: auto|wgc|dda", a.native.capture);
                }
            }
            "--intra-refresh" => a.native.intra_refresh = val()?.parse()?,
            "--sync-encode" => a.native.sync_encode = true,
            "--vbv" => a.native.vbv_frames = val()?.parse()?,
            "--split" => {
                a.native.split = match val()?.as_str() {
                    "auto" => 0,
                    "forced" => 1,
                    "two" => 2,
                    "off" => 15,
                    o => bail!("split {o}: auto|forced|two|off"),
                }
            }
            "--grab-fps" => a.grab_fps = Some(val()?.parse()?),
            "--no-cuda-upload" => a.cuda_upload = false,
            "--no-mouse" => a.no_mouse = true,
            "--audio" => a.audio = AudioMode::parse(&val()?).map_err(anyhow::Error::msg)?,
            "--audio-sink" => a.audio_sink = SinkChoice::parse(&val()?),
            "--duration" => a.duration = Some(val()?.parse()?),
            "--ffmpeg" => a.ffmpeg = Some(PathBuf::from(val()?)),
            "--verbose" | "-v" => a.verbosity = Verbosity::Verbose,
            "--reprobe" => a.cache = ProbeCache::Ignore,
            "--probe-dxgi" => a.cmd.probe_dxgi = true,
            "doctor" | "--doctor" => a.cmd.doctor = true,
            "caps" => a.cmd.caps = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            o => bail!("unknown argument {o}\n{USAGE}"),
        }
    }
    check_args(&mut a)?;
    Ok(a)
}

/// Cross-flag checks and clamps after parsing.
fn check_args(a: &mut Args) -> Result<()> {
    if let Some(e) = a.encoder {
        if e.encoder(a.codec()).is_none() && !a.cmd.doctor && !a.cmd.caps {
            bail!("--encoder {} cannot encode {}", e.name(), a.codec().name());
        }
    }
    if let Some(p) = &a.preset {
        if let Ok(n) = p.trim_start_matches(['p', 'P']).parse::<u8>() {
            a.native.preset = n.clamp(1, 7);
        }
    }
    if a.fps > 60 {
        eprintln!("[warn] --fps {} -> 60: TunnelRez runs at 60 Hz only", a.fps);
        a.fps = 60;
    }
    if a.backend_native == Some(true) {
        if !native::supports(a.codec()) {
            bail!("--backend native: H.264, HEVC or AV1 only");
        }
        if a.encoder.is_some() {
            bail!("--encoder names an ffmpeg encoder; leave it out with --backend native");
        }
    }
    if !a.cmd.probe_dxgi && !a.cmd.list && !a.cmd.doctor && !a.cmd.caps && a.tv.is_none() {
        bail!("--tv is required (e.g. --tv $env:TV_HOST)\n{USAGE}");
    }
    Ok(())
}

// ---------------------------------------------------------------- monitors

fn list_monitors(v: &Views) {
    let vdd = tunnelrez_vdd::vdd_display_names();
    println!("Monitors (index for --monitor; capture views: igpu = power-saving process, dgpu = high-performance):");
    for m in &v.igpu {
        println!(
            "  [{}] {:<14} {}x{} at ({},{}){}{}",
            m.index,
            m.device_name,
            m.width,
            m.height,
            m.x,
            m.y,
            if m.x == 0 && m.y == 0 { "  primary" } else { "" },
            if vdd.contains(&m.device_name) { "  virtual(Parsec VDD)" } else { "" },
        );
        for (name, list) in [("igpu", &v.igpu), ("dgpu", &v.dgpu)] {
            if let Some(l) = list.iter().find(|l| l.device_name == m.device_name) {
                println!(
                    "        {name}: adapter {} {} output {} dup={}",
                    l.adapter_idx,
                    monitors::vendor_name(l.vendor_id),
                    l.output_idx,
                    match &l.dup {
                        Ok(()) => "ok".to_string(),
                        Err(e) => e.clone(),
                    }
                );
            }
        }
    }
}

fn pick_monitor(t: &Tools, a: &Args, vdd: &mut Option<tunnelrez_vdd::VirtualDisplay>) -> Result<Monitor> {
    let mons = t.monitors(GpuPref::PowerSaving)?;
    if !a.extend {
        return match a.monitor {
            Some(i) => mons.get(i).cloned().ok_or_else(|| anyhow!("monitor {i} not found; see --list-monitors")),
            None => Ok(mons.iter().find(|m| m.x == 0 && m.y == 0).unwrap_or(&mons[0]).clone()),
        };
    }
    // Extend mode: reuse an existing Parsec virtual display, plug one only if none exists.
    let name = if let Some(n) = tunnelrez_vdd::vdd_display_names().into_iter().next() {
        eprintln!("[extend] using existing virtual display {n}");
        // Reused monitor: switch it to the requested size (60 Hz) unless it already runs it. `--size native` keeps
        // what it shows, but a detached monitor shows nothing: it must be attached either way.
        let mode = extend_mode(a.size);
        let ready = if a.size.is_some() {
            tunnelrez_vdd::ensure_mode(&n, mode)
        } else {
            tunnelrez_vdd::ensure_attached(&n, mode)
        };
        ready.map_err(|e| anyhow!("could not switch the TV monitor to {mode} ({n}): {e}"))?;
        n
    } else {
        let mode = extend_mode(a.size);
        let d = tunnelrez_vdd::VirtualDisplay::plug(mode).map_err(|e| anyhow!("plug virtual display: {e}"))?;
        let n = d.device_name.clone().ok_or_else(|| anyhow!("virtual display did not attach"))?;
        eprintln!("[extend] plugged virtual display {n} ({mode})");
        *vdd = Some(d);
        n
    };
    // DXGI sees the new mode a little after GDI: wait for the size we asked for.
    let want = a.size.map(|_| extend_mode(a.size));
    let mut seen = None;
    for _ in 0..50 {
        // The probe can find no outputs for a moment while Windows switches the mode: keep waiting.
        if let Some(m) =
            t.monitors(GpuPref::PowerSaving).unwrap_or_default().into_iter().find(|m| m.device_name == name)
        {
            if want.is_none_or(|w| size_matches(&m, w)) {
                return Ok(m);
            }
            seen = Some((m.width, m.height));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match (seen, want) {
        (Some((w, h)), Some(mode)) => {
            bail!("could not switch the TV monitor to {mode} ({name}): DXGI still shows {w}x{h}")
        }
        _ => bail!("virtual display {name} not visible to DXGI"),
    }
}

fn size_matches(m: &Monitor, mode: tunnelrez_vdd::Mode) -> bool {
    u32::try_from(m.width) == Ok(mode.width) && u32::try_from(m.height) == Ok(mode.height)
}

/// The adapter that renders `m`.
fn render_adapter<'a>(views: &'a Views, m: &Monitor) -> Option<&'a Monitor> {
    let is_virtual = tunnelrez_vdd::vdd_display_names().iter().any(|n| n.eq_ignore_ascii_case(&m.device_name));
    views.render_adapter(&m.device_name, if is_virtual { Display::Virtual } else { Display::Physical })
}

/// Log which adapter renders the captured monitor (name + LUID, see [`Views::render_adapter`]). Warns when it is not
/// NVIDIA: native NVENC still runs when picked, with a copy between the GPUs. Returns true for NVIDIA.
fn log_render_adapter(m: &Monitor, r: Option<&Monitor>) -> bool {
    let Some(r) = r else {
        eprintln!("[gpu] {}: render adapter unknown", m.device_name);
        return false;
    };
    eprintln!(
        "[gpu] {} is rendered by {} ({}, luid {:#x})",
        m.device_name,
        r.adapter_name,
        monitors::vendor_name(r.vendor_id),
        r.luid
    );
    if r.vendor_id != VENDOR_NVIDIA {
        eprintln!(
            "[warn] {} is not on the NVIDIA GPU: frames must cross GPUs, so native NVENC is slower. For the Parsec \
             virtual monitor, press Repair (`vdd repair --restart-adapter`): the driver reads \
             PreferredRenderAdapterVendorId=0x10DE only when its adapter starts (see README, setup).",
            m.device_name
        );
    }
    r.vendor_id == VENDOR_NVIDIA
}

/// The mode `--extend` wants for the virtual monitor: the requested size (1920x1080 for
/// `--size native`), always 60 Hz.
fn extend_mode(size: Option<(u32, u32)>) -> tunnelrez_vdd::Mode {
    let (w, h) = size.unwrap_or((1920, 1080));
    tunnelrez_vdd::Mode { width: w, height: h, hz: 60 }
}

// ---------------------------------------------------------------- plans

struct Ctx {
    w: u32,
    h: u32,
    fps: u32,
    codec: Codec,
    bitrate: u64,
}

fn make_plan(tools: &Tools, a: &Args, cx: &Ctx, b: Backend, c: &Capture, p: Path) -> Plan {
    let want = a.rate_mode();
    // Ask ffmpeg about -cbr_padding only when it matters (one `ffmpeg -h` per run, cached).
    let nvenc_padding =
        if b == Backend::Nvenc && want == RateMode::Fixed && b.encoder(cx.codec).is_some_and(|e| tools.encoder_pads(e))
        {
            NvencPadding::Supported
        } else {
            NvencPadding::Missing
        };
    Plan {
        backend: b,
        codec: cx.codec,
        cap: c.clone(),
        path: p,
        w: cx.w,
        h: cx.h,
        fps: cx.fps,
        bitrate: cx.bitrate,
        rate: encoder::effective_rate(want, b, nvenc_padding),
        threads: a.threads.unwrap_or_else(encoder::default_threads),
        preset: a.preset.clone(),
        // QSV rejects the 4x ddagrab rate the select-based modes signal, so it always uses dup.
        pacing: match if b == Backend::Qsv { "dup" } else { a.pacing.as_str() } {
            "dup" => Pacing { dup_frames: true, grab_fps: a.grab_fps.unwrap_or(cx.fps), select_fps: None },
            p => Pacing {
                dup_frames: p == "smooth",
                grab_fps: a.grab_fps.unwrap_or(cx.fps * 4),
                select_fps: Some(cx.fps),
            },
        },
        draw_mouse: !a.no_mouse,
        cuda_upload: a.cuda_upload,
    }
}

fn even(x: u32) -> u32 {
    x & !1
}

fn ctx_for(a: &Args, m: &Monitor, codec: Codec) -> Ctx {
    // Same bits as the old `as u32`; DXGI sizes are never negative.
    let (w, h) = a.size.unwrap_or((m.width.cast_unsigned(), m.height.cast_unsigned()));
    let (w, h) = (even(w), even(h));
    Ctx { w, h, fps: a.fps, codec, bitrate: encoder::target_bitrate(a.bitrate, codec, w, h, a.fps) }
}

/// Probe for a working plan. Auto mode falls back to H.264 if the requested codec has no working
/// encoder (e.g. HEVC without a GPU encoder on a CPU too slow for x265).
fn select(
    t: &Tools,
    a: &Args,
    m: &Monitor,
    caps: &[Capture],
    codec: Codec,
    banned: &HashSet<(Backend, Path)>,
) -> Result<(Plan, Ctx, String)> {
    let mut codecs = vec![codec];
    if a.encoder.is_none() && codec != Codec::H264 {
        codecs.push(Codec::H264);
    }
    for (i, &c) in codecs.iter().enumerate() {
        let cx = ctx_for(a, m, c);
        if c == Codec::H264 {
            if let Err(e) = level::h264_level_for(cx.w, cx.h, cx.fps, cx.bitrate) {
                if i == 0 {
                    return Err(e);
                }
                eprintln!("[encoder] no H.264 fallback: {e:#}");
                continue;
            }
        }
        let order = match a.encoder {
            Some(b) => vec![b],
            None => encoder::auto_order(c),
        };
        let key = probe::cache_key(t, c, cx.w, cx.h, cx.fps, &m.device_name, caps);
        let use_cache = a.cache == ProbeCache::Use;
        let mk = |b: Backend, cap: &Capture, p: Path| make_plan(t, a, &cx, b, cap, p);
        if let Some(plan) =
            probe::choose(t, &order, caps, cx.w, cx.h, use_cache.then_some(key.as_str()), banned, &mk, a.verbosity)
        {
            if i > 0 {
                eprintln!("[encoder] no working {} encoder; using {} instead", codec.name(), c.name());
            }
            return Ok((plan, cx, key));
        }
    }
    bail!("no working encoder for {} (run `tunnelrez-send doctor` to see why)", codec.name())
}

// ---------------------------------------------------------------- doctor

fn doctor(t: &Tools, a: &Args, m: &Monitor, caps: &[Capture]) -> Result<()> {
    let ffmpeg = t.ffmpeg_src.as_ref().context("ffmpeg.exe not on PATH (use --ffmpeg PATH)")?;
    let ver = Command::new(ffmpeg)
        .arg("-version")
        .creation_flags(0x0800_0000)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string())
        .unwrap_or_default();
    println!("ffmpeg: {ver}");
    println!("monitor: {} {}x{}", m.device_name, m.width, m.height);
    for c in caps {
        println!(
            "  capture view {}: {} ({}) output {}",
            c.pref.name(),
            monitors::vendor_name(c.vendor),
            c.adapter,
            c.output_idx
        );
    }
    if caps.is_empty() {
        bail!("no GPU can duplicate {} (see --list-monitors)", m.device_name);
    }
    let codecs: Vec<Codec> = match a.codec_arg {
        Some(c) => vec![c],
        None => vec![Codec::H264, Codec::Hevc, Codec::Av1],
    };
    let cx0 = ctx_for(a, m, codecs[0]);
    println!(
        "target: {}x{}@{}, {} measured frames each after {} warm-up, IDR requested half way",
        cx0.w,
        cx0.h,
        cx0.fps,
        probe::MEASURE_FRAMES,
        probe::WARMUP_FRAMES
    );
    println!();
    println!("{}", probe::DOCTOR_HEADER);
    let dead = probe::DeadViews::default();
    for c in codecs {
        let cx = ctx_for(a, m, c);
        let mk = |b: Backend, cap: &Capture, p: Path| make_plan(t, a, &cx, b, cap, p);
        probe::doctor_rows(t, c, caps, cx.w, cx.h, &mk, &dead, a.encoder, a.verbosity);
    }
    println!();
    println!(
        "lat_ms: latency, capture -> encoded frame out of ffmpeg (not time per frame: a hardware encoder works on \
         several frames at once). fps decides if it keeps up. cores: ffmpeg CPU use (1.0 = one logical CPU)."
    );
    Ok(())
}

// ---------------------------------------------------------------- caps

/// The encoders `caps` reports, in the app's words: `native` is the in-process NVENC SDK backend, `cpu`
/// the software encoder for the codec (x264 / x265 / SVT-AV1).
const CAPS_ENCODERS: [&str; 6] = ["native", "nvenc", "qsv", "amf", "mf", "cpu"];

/// The ffmpeg backend behind a `caps` encoder name for `c` (None for `native`).
fn caps_backend(name: &str, c: Codec) -> Option<Backend> {
    match name {
        "cpu" => Some(encoder::software_for(c)),
        n => Backend::parse(n),
    }
}

/// False when `caps` measured the native backend for this codec, size and rate and it failed.
fn native_not_ruled_out(t: &Tools, a: &Args, m: &Monitor, caps: &[Capture]) -> bool {
    if a.cache == ProbeCache::Ignore {
        return true; // --reprobe: try native again
    }
    let cx = ctx_for(a, m, a.codec());
    let key = probe::cache_key(t, a.codec(), cx.w, cx.h, cx.fps, &m.device_name, caps);
    match probe::caps_cached(&probe::caps_key("native", &key)) {
        Some(r) if !r.ok => {
            eprintln!("[encoder] native backend skipped (caps: {})", r.reason);
            false
        }
        _ => true,
    }
}

/// A real native encode of codec `c` on monitor `m`, whichever GPU renders it: only a failed encode counts.
fn native_works(a: &Args, m: &Monitor, c: Codec) -> probe::CapResult {
    let fail = |reason: String| probe::CapResult { ok: false, reason, cached: false };
    if !native::available() {
        return fail("this build has no native backend".into());
    }
    if !native::supports(c) {
        return fail(format!("the native backend does not encode {}", c.name()));
    }
    let cfg = match native_cfg_for(&Args { codec_arg: Some(c), ..a.clone() }, m) {
        Ok(cfg) => cfg,
        Err(e) => return fail(probe::short_reason(&format!("{e:#}"))),
    };
    let mut src = match native::start(&cfg) {
        Ok(s) => s,
        Err(e) => return fail(probe::short_reason(&format!("{e:#}"))),
    };
    let t0 = Instant::now();
    let mut first: Option<Instant> = None;
    let mut enc_ms: Vec<f32> = Vec::new();
    let mut csc_ms: Vec<f32> = Vec::new();
    let r = loop {
        match src.next() {
            Ok(Some(source::Event::Frame(au))) => {
                let t1 = *first.get_or_insert_with(Instant::now);
                let ms = |a: u64, b: u64| num::f32_from_u64(a.saturating_sub(b)) / 1000.0;
                if let Some(g) = au.stages {
                    // Colour conversion runs only for new screen content: keep every sample, warm-up included.
                    if !g.repeat {
                        csc_ms.push(ms(g.convert_done_us, g.arrive_us) + num::f32_from_u64(g.xfer_us) / 1000.0);
                    }
                    // After a 0.5 s warm-up, sample the NVENC time of every picture for 1.5 s.
                    if t1.elapsed() >= NATIVE_WARMUP {
                        enc_ms.push(ms(au.arrive_us, g.encode_start_us));
                    }
                }
                if t1.elapsed() >= NATIVE_WARMUP + NATIVE_MEASURE {
                    break native_verdict(&csc_ms, &enc_ms, cfg.fps);
                }
            }
            Ok(Some(_)) if t0.elapsed() < Duration::from_secs(5) => {}
            Ok(_) if first.is_some() => break native_verdict(&csc_ms, &enc_ms, cfg.fps),
            Ok(_) => break fail("no frame within 5 s".into()),
            Err(e) => break fail(probe::short_reason(&format!("{e:#}"))),
        }
    };
    src.stop(source::StopMode::Kill);
    r
}

/// Native `caps` warm-up and measuring time.
const NATIVE_WARMUP: Duration = Duration::from_millis(500);
const NATIVE_MEASURE: Duration = Duration::from_millis(1500);

/// Native `caps` result: the GPU work per new picture, colour conversion (with any copy between GPUs) plus NVENC (means,
/// ms), must fit one frame period at `fps`. The stages share the GPU, so their sum is the conservative per-picture
/// cost. The output frame rate is not a test here: on a still desktop the native backend sends only repeats (every
/// 100 ms, no conversion), so the stage times are what show whether it can sustain the rate.
fn native_verdict(csc_ms: &[f32], enc_ms: &[f32], fps: u32) -> probe::CapResult {
    let budget = 1000.0 / num::f32_from_u32(fps.max(1));
    if enc_ms.len() < 5 || csc_ms.is_empty() {
        let reason = format!("only {} pictures measured", enc_ms.len());
        return probe::CapResult { ok: false, reason, cached: false };
    }
    // Means, not medians: throughput is total work / pictures, so a few very slow pictures count in full.
    let mean = |v: &[f32]| v.iter().sum::<f32>() / num::f32_from_usize(v.len());
    let (csc, enc) = (mean(csc_ms), mean(enc_ms));
    if csc + enc > budget {
        let reason = format!("too slow: {csc:.1} ms convert + {enc:.1} ms encode per picture, budget {budget:.1} ms");
        return probe::CapResult { ok: false, reason, cached: false };
    }
    probe::CapResult { ok: true, reason: String::new(), cached: false }
}

/// `caps`: one JSON object with every (encoder, codec) result at the requested size and rate.
fn caps_report(t: &Tools, a: &Args, m: &Monitor, caps: &[Capture]) -> String {
    let mut out = Vec::new();
    let mut size = String::new();
    for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
        let cx = ctx_for(a, m, c);
        size = format!("{}x{}@{}", cx.w, cx.h, cx.fps);
        let key = probe::cache_key(t, c, cx.w, cx.h, cx.fps, &m.device_name, caps);
        for name in CAPS_ENCODERS {
            let entry = probe::caps_key(name, &key);
            // Known without ffmpeg (and not cached: installing the driver must take effect at once).
            let unavailable = caps_backend(name, c).and_then(probe::backend_unavailable).map(|why| probe::CapResult {
                ok: false,
                reason: why.into(),
                cached: false,
            });
            let cached = (a.cache == ProbeCache::Use).then(|| probe::caps_cached(&entry)).flatten();
            let r = unavailable.or(cached).unwrap_or_else(|| {
                let r = match (name, caps_backend(name, c)) {
                    ("native", _) => native_works(a, m, c),
                    (_, Some(b)) if b.encoder(c).is_some() => {
                        let mk = |b: Backend, cap: &Capture, p: Path| make_plan(t, a, &cx, b, cap, p);
                        probe::backend_works(t, b, caps, cx.w, cx.h, &mk, a.verbosity)
                    }
                    _ => probe::CapResult { ok: false, reason: format!("no {} encoder", c.name()), cached: false },
                };
                probe::caps_remember(&entry, &r);
                r
            });
            eprintln!("[caps] {name} {}: {}", c.name(), if r.ok { "ok" } else { r.reason.as_str() });
            out.push(serde_json::json!({
                "encoder": name, "codec": c.name(), "ok": r.ok, "reason": r.reason, "cached": r.cached,
            }));
        }
    }
    serde_json::json!({ "size": size, "results": out }).to_string()
}

// ---------------------------------------------------------------- main

/// Starts (or restarts) the encoder for one session.
type MakeSource<'a> = Box<dyn FnMut() -> Result<Box<dyn Source>> + 'a>;

/// Hidden debug mode: `tunnelrez-send --run-ffmpeg [--dgpu] [--no-timer] <ffmpeg args...>` runs
/// ffmpeg-igpu.exe (or -dgpu) with the same timer-resolution/priority treatment the sender gives it.
fn run_ffmpeg_debug(mut rest: Vec<String>) -> Result<()> {
    let t = Tools::setup(None)?;
    let mut pref = GpuPref::PowerSaving;
    let mut timer = true;
    while let Some(f) = rest.first().cloned() {
        match f.as_str() {
            "--dgpu" => pref = GpuPref::HighPerformance,
            "--no-timer" => timer = false,
            _ => break,
        }
        rest.remove(0);
    }
    let mut c = Command::new(t.ffmpeg(pref)?).args(&rest).creation_flags(0x80).spawn()?;
    if timer {
        winutil::high_res_timer_child(&c)?;
    }
    c.wait()?;
    Ok(())
}

/// The encoder choice for the stream: native NVENC or an ffmpeg plan, plus the ffmpeg
/// (backend, path) pairs that failed at runtime.
struct Choice {
    native_cfg: Option<NativeCfg>,
    chosen: Option<(Plan, Ctx, String)>,
    banned: HashSet<(Backend, Path)>,
}

/// The native NVENC config for monitor `m`.
fn native_cfg_for(a: &Args, m: &Monitor) -> Result<NativeCfg> {
    let cx = ctx_for(a, m, a.codec());
    let h264_level = match a.codec() {
        Codec::H264 => Some(level::h264_level_for(cx.w, cx.h, cx.fps, cx.bitrate)?),
        _ => None,
    };
    Ok(NativeCfg {
        output: m.device_name.clone(),
        codec: a.codec(),
        width: cx.w,
        height: cx.h,
        fps: cx.fps,
        bitrate_bps: cx.bitrate,
        rate: a.rate_mode(),
        h264_level,
        opts: a.native.clone(),
    })
}

/// Session config and encoder factory for the current choice.
fn session_for<'a>(a: &Args, t: &'a Tools, m: &Monitor, ch: &Choice) -> (session::Cfg, MakeSource<'a>) {
    match (&ch.native_cfg, &ch.chosen) {
        (Some(nc), _) => {
            let nc = nc.clone();
            (
                session::Cfg {
                    codec: nc.codec,
                    w: nc.width,
                    h: nc.height,
                    fps: nc.fps,
                    label: format!("{}/native", nc.codec.name()),
                    target_bps: nc.bitrate_bps,
                    requested_bps: a.bitrate,
                    rate: nc.rate,
                    verbose: a.verbosity.is_verbose(),
                    audio: a.audio,
                    audio_sink: a.audio_sink.clone(),
                },
                Box::new(move || native::start(&nc)),
            )
        }
        (None, Some((plan, _, _))) => {
            eprintln!("[capture] {} -> {}", m.device_name, plan.describe());
            if plan.rate != a.rate_mode() {
                eprintln!(
                    "[encoder] {} cannot pad here (QSV/MF/x265/SVT-AV1, or ffmpeg older than 8 for NVENC): \
                     the bitrate is a ceiling",
                    plan.backend.name()
                );
            }
            let (plan, verbose) = (plan.clone(), a.verbosity.is_verbose());
            (
                session::Cfg {
                    codec: plan.codec,
                    w: plan.w,
                    h: plan.h,
                    fps: plan.fps,
                    label: format!("{}/{} {}", plan.codec.name(), plan.backend.name(), plan.path.name()),
                    target_bps: plan.bitrate,
                    requested_bps: a.bitrate,
                    rate: plan.rate,
                    verbose,
                    audio: a.audio,
                    audio_sink: a.audio_sink.clone(),
                },
                Box::new(move || {
                    let s = FfmpegSource::spawn(t, &plan, &RunOpts { verbose, frames: None, echo_stderr: true })?;
                    let src: Box<dyn Source> = Box::new(s);
                    Ok(src)
                }),
            )
        }
        _ => unreachable!(),
    }
}

/// The session gave up on the encoder: fall back from native to ffmpeg, re-plan after a display
/// layout change, or demote the failed ffmpeg plan and pick the next one.
fn on_encoder_failed(
    why: &str,
    t: &Tools,
    a: &Args,
    m: &mut Monitor,
    caps: &mut Vec<Capture>,
    ch: &mut Choice,
) -> Result<()> {
    if ch.native_cfg.is_some() {
        if a.backend_native == Some(true) {
            bail!("native encoder failed {} times in a row: {why}", session::MAX_ENCODER_FAILS);
        }
        eprintln!(
            "[encoder] native backend failed {} times in a row ({why}); falling back to ffmpeg",
            session::MAX_ENCODER_FAILS
        );
        ch.native_cfg = None;
        *caps = t.views().map(|v| v.captures(&m.device_name)).unwrap_or_default();
        if caps.is_empty() {
            bail!("no GPU can duplicate {} for ffmpeg (see --list-monitors)", m.device_name);
        }
        ch.chosen = Some(select(t, a, m, caps, a.codec(), &ch.banned)?);
        return Ok(());
    }
    let Some((plan, _, key)) = ch.chosen.take() else { bail!("no encoder chosen") };
    // A monitor was plugged/unplugged: output indices shift and ddagrab fails
    // with ACCESS_LOST / "Failed to enumerate DXGI output". Find our monitor
    // again by device name; if its capture views changed, re-plan without
    // demoting the encoder.
    let new_caps = t.views().map(|v| v.captures(&m.device_name)).unwrap_or_default();
    if !new_caps.is_empty() && new_caps != *caps {
        eprintln!("[capture] display layout changed; capturing {} again", m.device_name);
        if let Some(nm) = t.monitors(GpuPref::PowerSaving)?.into_iter().find(|x| x.device_name == m.device_name) {
            *m = nm;
        }
        *caps = new_caps;
        ch.chosen = Some(select(t, a, m, caps, plan.codec, &ch.banned)?);
        return Ok(());
    }
    if a.encoder.is_some() {
        bail!("encoder {} failed {} times in a row: {why}", plan.backend.name(), session::MAX_ENCODER_FAILS);
    }
    eprintln!(
        "[encoder] demoting {} {} after {} failures: {why}",
        plan.backend.name(),
        plan.path.name(),
        session::MAX_ENCODER_FAILS
    );
    probe::forget(&key);
    ch.banned.insert((plan.backend, plan.path));
    ch.chosen = Some(select(t, a, m, caps, plan.codec, &ch.banned)?);
    Ok(())
}

/// `vdd status | unplug | repair [--dry-run] [--restart-adapter] | install [--dry-run] [--force]` (the TV monitor driver), and
/// the elevated child `vdd elevated <action> <installer> <result file>` that they start with one UAC prompt.
fn vdd_command(args: &[String]) -> Result<String> {
    use tunnelrez::vdd_driver::{self as vdd, Reinstall, Restart};
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let restart = if a.contains(&"--restart-adapter") { Restart::Always } else { Restart::IfNeeded };
    let how = if a.contains(&"--force") { Reinstall::Force } else { Reinstall::IfMissing };
    let dry = a.contains(&"--dry-run");
    let known = |flags: &[&str]| a[1..].iter().all(|f| flags.contains(f));
    Ok(match a.first().copied() {
        Some("status") if a.len() == 1 => vdd::status_json()?,
        // The uninstaller runs this: no TV monitor may stay behind. No UAC prompt (the control interface is per user).
        Some("unplug") if a.len() == 1 => {
            tunnelrez_vdd::unplug_all().map_err(|e| anyhow!("{e}"))?;
            "every TV monitor unplugged".into()
        }
        Some("repair") if known(&["--dry-run", "--restart-adapter"]) => {
            if dry {
                vdd::repair_plan(restart)?
            } else {
                vdd::repair_command(restart)?
            }
        }
        Some("install") if known(&["--dry-run", "--force"]) => {
            if dry {
                vdd::install_plan(how)?
            } else {
                vdd::install_command(how)?
            }
        }
        Some("elevated") if a.len() == 4 => std::process::exit(vdd::elevated_child(a[1], a[2], a[3])),
        _ => bail!(
            "usage: tunnelrez-send vdd status | unplug | repair [--dry-run] [--restart-adapter] | install [--dry-run] [--force]"
        ),
    })
}

/// `audio-probe`, `audio-restore`, `audio-devices`, `audio-sink ...` and `vdd ...`: their output, or None for any other
/// command line.
fn audio_command(argv: &[String]) -> Option<Result<String>> {
    match argv.get(1)?.as_str() {
        "vdd" => Some(vdd_command(&argv[2..])),
        "audio-sink" => Some(audio_sink_command(&argv[2..])),
        "audio-probe" => Some(tunnelrez::audio::probe()),
        "audio-restore" => Some(Ok(tunnelrez::audio::restore_all())),
        "audio-devices" => Some(tunnelrez::audio::devices_json()),
        _ => None,
    }
}

/// `audio-sink status | install [--dry-run] | uninstall [--force]` (the TV speaker driver), and the elevated
/// child `audio-sink elevated <action> <inf> <result file>` that `install`/`uninstall` start with one UAC prompt.
fn audio_sink_command(args: &[String]) -> Result<String> {
    use tunnelrez::audio::driver;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match a.as_slice() {
        ["status"] => driver::status_json()?,
        ["install", "--dry-run"] => driver::plan()?,
        ["install"] => driver::install_command()?,
        ["uninstall"] => driver::uninstall_command(driver::Remove::Ours)?,
        ["uninstall", "--force"] => driver::uninstall_command(driver::Remove::Any)?,
        ["restore-defaults"] => driver::recover_stale().unwrap_or_else(|| "nothing to restore".into()),
        ["elevated", action, inf, result] => std::process::exit(driver::elevated_child(action, inf, result)),
        _ => bail!(
            "usage: tunnelrez-send audio-sink status | install [--dry-run] | uninstall [--force] | restore-defaults"
        ),
    })
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(std::string::String::as_str) == Some("--run-ffmpeg") {
        return run_ffmpeg_debug(argv[2..].to_vec());
    }
    if let Some(out) = audio_command(&argv) {
        println!("{}", out?);
        return Ok(());
    }
    let a = parse_args()?;
    if a.cmd.probe_dxgi {
        for m in monitors::list(monitors::DupProbe::Probe)? {
            println!("{}", monitors::to_line(&m));
        }
        return Ok(());
    }
    winutil::high_res_timer_self();
    let t = Tools::setup(a.ffmpeg.as_deref())?;
    if a.cmd.list {
        list_monitors(&t.views()?);
        return Ok(());
    }
    let mut vdd = None;
    let mut m = pick_monitor(&t, &a, &mut vdd)?;
    let views = t.views()?;
    let mut caps = views.captures(&m.device_name);
    let on_nvidia = log_render_adapter(&m, render_adapter(&views, &m));
    if a.cmd.doctor {
        return doctor(&t, &a, &m, &caps);
    }
    if a.cmd.caps {
        println!("{}", caps_report(&t, &a, &m, &caps));
        return Ok(());
    }
    ctrlc::set_handler(|| {
        if STOP.swap(true, SeqCst) {
            std::process::exit(130);
        }
    })?;
    for m in tunnelrez::audio::recover_stale() {
        eprintln!("[audio] {m}");
    }
    // Puts the default playback device back when main returns, by any path (the virtual sink is held across
    // sessions).
    let _audio_route = tunnelrez::audio::sink::ExitGuard;
    let deadline = a.duration.map(|d| Instant::now() + Duration::from_secs_f64(d));
    let tv = a.tv.as_ref().context("--tv is required")?;
    let addr =
        format!("{tv}:{}", a.port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("cannot resolve TV address"))?;

    // Encoder choice.
    // Native NVENC first: asked for, or `--encoder auto` with an NVIDIA GPU rendering the monitor. On another GPU
    // the copy Windows makes before a captured frame arrives cannot be measured on a still screen, so `caps` saying
    // ok is no proof that it keeps up: Auto leaves that path to an explicit pick.
    let want_native = match a.backend_native {
        Some(b) => b,
        None => {
            a.encoder.is_none()
                && native::available()
                && native::supports(a.codec())
                && on_nvidia
                && native_not_ruled_out(&t, &a, &m, &caps)
        }
    };
    let native_cfg = if want_native { Some(native_cfg_for(&a, &m)?) } else { None };
    let mut ch = Choice { native_cfg, chosen: None, banned: HashSet::new() };
    if ch.native_cfg.is_some() {
        if !native::available() {
            bail!("--backend native is not in this build (built without the `native` feature); use --backend ffmpeg");
        }
        eprintln!("[encoder] native NVENC backend");
    } else {
        if caps.is_empty() {
            bail!("no GPU can duplicate {} for ffmpeg (see --list-monitors)", m.device_name);
        }
        ch.chosen = Some(select(&t, &a, &m, &caps, a.codec(), &ch.banned)?);
    }

    let mut waiting = false;
    while !STOP.load(SeqCst) && deadline.is_none_or(|d| Instant::now() < d) {
        let (cfg, mut make) = session_for(&a, &t, &m, &ch);
        match session::connect(addr) {
            Ok(s) => {
                eprintln!("[net] connected to {addr}");
                waiting = false;
                match session::run(s, &cfg, deadline, &mut *make)? {
                    End::Stop => break,
                    End::Disconnected => {
                        eprintln!("[net] disconnected; reconnecting");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    End::EncoderFailed(why) => {
                        drop(make);
                        on_encoder_failed(&why, &t, &a, &mut m, &mut caps, &mut ch)?;
                    }
                }
            }
            Err(e) => {
                if !waiting {
                    eprintln!("[net] waiting for TV at {addr} ({e})");
                    waiting = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    drop(vdd);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rate_follows_an_explicit_bitrate() {
        let mut a = Args::default();
        assert_eq!(a.rate_mode(), RateMode::Ceiling, "no --bitrate: the default is a ceiling");
        a.bitrate = Some(parse_rate("150M").unwrap());
        assert_eq!(a.bitrate, Some(150_000_000));
        assert_eq!(a.rate_mode(), RateMode::Fixed, "--bitrate given: fixed rate");
        a.fixed_rate = Some(RateMode::Ceiling);
        assert_eq!(a.rate_mode(), RateMode::Ceiling, "--fixed-rate off wins");
        let a = Args { fixed_rate: Some(RateMode::Fixed), ..Args::default() };
        assert_eq!(a.rate_mode(), RateMode::Fixed, "--fixed-rate on with the default bitrate");
    }

    #[test]
    fn default_4k60_hevc_bitrate_is_100m() {
        let m = monitors::from_line("MON\t0\t1\tGPU\t4318\t1a2b\t0\tV\t0\t0\t3840\t2160\tok").unwrap();
        let a = Args { size: Some((3840, 2160)), ..Args::default() };
        assert_eq!(ctx_for(&a, &m, Codec::Hevc).bitrate, 100_000_000);
        let a = Args { size: Some((3840, 2160)), bitrate: Some(150_000_000), ..Args::default() };
        assert_eq!(ctx_for(&a, &m, Codec::Hevc).bitrate, 150_000_000);
        let n = native_cfg_for(&Args { codec_arg: Some(Codec::Hevc), ..a }, &m).expect("hevc");
        assert_eq!((n.rate, n.h264_level), (RateMode::Fixed, None));
    }

    #[test]
    fn h264_and_fallback_use_the_bitrate_as_given() {
        let m = monitors::from_line("MON	0	1	GPU	4318	1a2b	0	V	0	0	3840	2160	ok").unwrap();
        // The app's launch with the slider at 150: the same rate for whatever codec ends up being used.
        let a = Args { size: Some((3840, 2160)), bitrate: Some(150_000_000), ..Args::default() };
        assert_eq!(ctx_for(&a, &m, Codec::H264).bitrate, 150_000_000, "H.264 (or HEVC -> H.264 fallback)");
        assert_eq!(ctx_for(&a, &m, Codec::Hevc).bitrate, 150_000_000);
        let n = native_cfg_for(&Args { codec_arg: Some(Codec::H264), ..a.clone() }, &m).expect("level 5.2 carries it");
        assert_eq!((n.bitrate_bps, n.h264_level), (150_000_000, Some(52)), "native H.264: no cap, level 5.2");
        let too_much = Args { codec_arg: Some(Codec::H264), bitrate: Some(900_000_000), ..a };
        let e = native_cfg_for(&too_much, &m).expect_err("beyond level 6.2").to_string();
        assert!(e.contains("900.0 Mb/s"), "fails with a clear message, never clamps: {e}");
    }

    #[test]
    fn native_caps_needs_a_sustained_encode() {
        assert!(native_verdict(&[0.5; 3], &[3.0; 20], 60).ok);
        // A first frame came, but NVENC needs 25 ms per picture: it cannot keep 60 fps.
        let r = native_verdict(&[0.5; 3], &[25.0; 20], 60);
        assert!(!r.ok && r.reason.starts_with("too slow"), "{r:?}");
        // Fast encode, slow conversion (22 + 3 ms = ~40 fps): rejected too.
        let r = native_verdict(&[22.0; 3], &[3.0; 20], 60);
        assert!(!r.ok && r.reason.contains("22.0 ms convert"), "{r:?}");
        assert!(!native_verdict(&[0.5], &[3.0; 2], 60).ok, "a single frame is not proof");
        // Uneven encode: mostly 3 ms, but every fourth picture takes 180 ms (~20 fps of work): rejected.
        let uneven: Vec<f32> = (0..20).map(|i| if i % 4 == 0 { 180.0 } else { 3.0 }).collect();
        assert!(!native_verdict(&[0.5; 3], &uneven, 60).ok);
    }

    #[test]
    fn extend_mode_follows_size_at_60_hz() {
        let m = extend_mode(Some((3840, 2160)));
        assert_eq!((m.width, m.height, m.hz), (3840, 2160, 60));
        let m = extend_mode(None);
        assert_eq!((m.width, m.height, m.hz), (1920, 1080, 60));
    }

    #[test]
    fn reused_monitor_must_show_the_requested_size() {
        let mut m = monitors::from_line("MON\t0\t1\tGPU\t4318\t1a2b\t0\tV\t0\t0\t1920\t1080\tok").unwrap();
        assert_eq!(m.luid, 0x1a2b);
        assert!(!size_matches(&m, extend_mode(Some((3840, 2160)))));
        m.width = 3840;
        m.height = 2160;
        assert!(size_matches(&m, extend_mode(Some((3840, 2160)))));
        assert_eq!(monitors::from_line(&monitors::to_line(&m)).map(|x| x.luid), Some(0x1a2b));
    }
}
