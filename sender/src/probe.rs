//! Encoder probing: run the real capture -> encode pipeline for a few frames, keep the first
//! backend that works, cache that choice on disk, and print the `doctor` table.
//!
//! Same pattern as `RustDesk`'s hwcodec / screenpipe: probe once, cache, and let the session demote
//! to the next backend after repeated runtime failures (see `session::MAX_ENCODER_FAILS`).

use crate::encoder::{self, Backend, Capture, GpuPref, Path, Plan};
use crate::ffmpeg::{FfmpegSource, RunOpts};
use crate::flv::Codec;
use crate::num::{f32_from_u128, f32_from_u32, f32_from_u64, f32_from_usize};
use crate::source::{Event, Source, StopMode};
use crate::stats::pct;
use crate::tools::Tools;
use crate::winutil;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Frames for the quick "does it work" probe.
pub const QUICK_FRAMES: u32 = 2;
/// Frames for a measuring probe (software encoders must show they keep up; doctor numbers),
/// after `WARMUP_FRAMES` (the start-up backlog drains in the first ~0.5 s).
pub const MEASURE_FRAMES: u32 = 90;
pub const WARMUP_FRAMES: u32 = 30;

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub ok: bool,
    pub reason: String,
    pub frames: u32,
    /// Latency, ms: capture -> encoded frame out of ffmpeg (after warm-up). Not the time per frame: a
    /// pipelined hardware encoder holds several frames at once, so this can be well above the frame period
    /// while the encoder keeps up.
    pub lat_p50: f32,
    pub lat_p99: f32,
    /// Encoded frames per second (after warm-up): the throughput test.
    pub fps: f32,
    /// ffmpeg CPU use in cores (1.0 = one logical CPU busy)
    pub cpu_cores: f32,
    /// spawn -> first frame, ms
    pub startup_ms: f32,
    /// None: not tested
    pub idr_ok: Option<bool>,
}

/// How much a probe prints.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verbosity {
    Quiet,
    /// Print the ffmpeg command line and echo its stderr (`--verbose`).
    Verbose,
}

impl Verbosity {
    pub fn is_verbose(self) -> bool {
        self == Verbosity::Verbose
    }
}

/// Whether [`run`] also asks for an IDR half way through and checks that one comes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdrTest {
    Test,
    Skip,
}

/// The probe failed in capture (ddagrab), not in the encoder: skip that capture view.
pub fn capture_failed(reason: &str) -> bool {
    reason.contains("AcquireNextFrame") || reason.contains("DuplicateOutput") || reason.contains("887a00")
}

/// Kill process `pid` after `timeout` unless the returned flag is set first (a hung encoder init
/// must not hang the probe).
fn spawn_watchdog(pid: u32, timeout: Duration) -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    std::thread::spawn(move || {
        let t = Instant::now();
        while t.elapsed() < timeout {
            if flag.load(SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Check again: once the probe is done the child handle is closed and the PID may be reused.
        if !flag.load(SeqCst) {
            winutil::kill_pid(pid);
        }
    });
    done
}

/// Run `plan` for `warmup` + `frames` frames (with [`IdrTest::Test`], ask for an IDR half way
/// through the measured part). Stats cover the measured part only.
pub fn run(t: &Tools, plan: &Plan, frames: u32, warmup: u32, idr: IdrTest, verbosity: Verbosity) -> Probe {
    let test_idr = idr == IdrTest::Test;
    let verbose = verbosity.is_verbose();
    let total = frames + warmup;
    let mut r = Probe::default();
    let timeout = Duration::from_millis(4000 + u64::from(total) * 3000 / u64::from(plan.fps.max(1)));
    let t0 = Instant::now();
    let mut src =
        match FfmpegSource::spawn(t, plan, &RunOpts { verbose, frames: Some(total + 5), echo_stderr: verbose }) {
            Ok(s) => s,
            Err(e) => {
                r.reason = format!("{e:#}");
                return r;
            }
        };
    // Watchdog: a hung encoder init must not hang the probe.
    let done = spawn_watchdog(src.pid(), timeout);
    let mut cpu0 = None;
    let mut t_meas = Instant::now();
    let mut lat = Vec::new();
    let mut arrivals = Vec::new();
    let mut first_key = None;
    let idr_frame = warmup + frames / 2;
    let mut ended = false;
    while r.frames < total {
        match src.next() {
            Ok(Some(Event::Frame(au))) => {
                if r.frames == 0 {
                    r.startup_ms = t0.elapsed().as_secs_f32() * 1000.0;
                    first_key = Some(au.key);
                }
                if r.frames == warmup {
                    cpu0 = src.cpu_time_us();
                    t_meas = Instant::now();
                }
                if r.frames >= warmup.max(1) {
                    lat.push(f32_from_u64(au.arrive_us.saturating_sub(au.cap_us)) / 1000.0);
                    arrivals.push(au.arrive_us);
                }
                if test_idr && r.frames > idr_frame && au.key && r.idr_ok.is_none() {
                    r.idr_ok = Some(true);
                }
                r.frames += 1;
                if test_idr && r.frames == idr_frame && !src.force_idr() {
                    r.idr_ok = Some(false);
                }
            }
            Ok(Some(Event::Config(_) | Event::Idle)) => {}
            Ok(None) | Err(_) => {
                ended = true;
                break;
            }
        }
    }
    let wall = t0.elapsed();
    let meas = t_meas.elapsed();
    let cpu1 = src.cpu_time_us();
    done.store(true, SeqCst);
    let reason = if ended { src.exit_reason() } else { String::new() };
    let last_err = src.last_error();
    Box::new(src).stop(StopMode::Kill);
    if test_idr && r.frames > idr_frame && r.idr_ok.is_none() {
        r.idr_ok = Some(false);
    }
    // In arrival order, for the falling-behind test (`pct` sorts its input).
    let lat_in_order = lat.clone();
    let (p50, p99, _) = pct(&mut lat);
    r.lat_p50 = p50;
    r.lat_p99 = p99;
    r.fps = fps_of(&arrivals);
    if let (Some(a), Some(b)) = (cpu0, cpu1) {
        r.cpu_cores = f32_from_u64(b.saturating_sub(a)) / f32_from_u128(meas.as_micros().max(1));
    }
    if r.frames < total {
        r.reason = if r.frames == 0 && wall >= timeout {
            "timed out (no frame)".into()
        } else if ended {
            if last_err.is_empty() {
                reason
            } else {
                last_err
            }
        } else {
            "no frames".into()
        };
        return r;
    }
    if first_key == Some(false) && plan.codec != Codec::Av1 {
        r.reason = "first frame is not a keyframe".into();
        return r;
    }
    // Software (and any slow) encoders must keep up with the frame rate.
    if frames >= 30 {
        if let Err(slow) = speed_verdict(r.fps, plan.fps, &lat_in_order) {
            r.reason = slow.to_string();
            return r;
        }
    }
    r.ok = true;
    r
}

/// Frames per second over arrival times (µs); 0 with 10 frames or fewer.
fn fps_of(arrivals: &[u64]) -> f32 {
    let (Some(first), Some(last)) = (arrivals.first(), arrivals.last()) else { return 0.0 };
    let span = f32_from_u64(last.saturating_sub(*first)) / 1e6;
    if arrivals.len() > 10 && span > 0.0 {
        f32_from_usize(arrivals.len() - 1) / span
    } else {
        0.0
    }
}

/// Why an encoder cannot keep up (see [`speed_verdict`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TooSlow {
    /// Throughput below 90 % of the target rate.
    Fps { got: f32, need: u32 },
    /// Latency grows: the encoder falls further behind with each frame.
    FallingBehind { from_ms: f32, to_ms: f32 },
}

impl std::fmt::Display for TooSlow {
    /// `too slow: ` plus a first comma-separated part the app can show.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            TooSlow::Fps { got, need } => write!(f, "too slow: {got:.0} fps, needs {need}"),
            TooSlow::FallingBehind { from_ms, to_ms } => {
                write!(f, "too slow: falling behind, latency {from_ms:.0} -> {to_ms:.0} ms")
            }
        }
    }
}

/// Median of a non-empty sample set.
fn median(v: &[f32]) -> f32 {
    let mut v = v.to_vec();
    pct(&mut v).0
}

/// Can the encoder sustain `target_fps`? `fps` is the measured throughput and `lat_ms` the capture -> output
/// latency of each measured frame in arrival order (start-up and warm-up already left out).
///
/// Throughput decides: `fps` must reach 90 % of the target. Latency alone does not fail an encoder (a pipelined
/// hardware encoder holds several frames, so its latency can be several frame periods while it keeps up), but a
/// latency that grows does: the median of the last quarter minus the median of the first quarter must stay
/// within two frame periods. A steady or falling latency (a start-up backlog draining) passes.
///
/// # Errors
///
/// [`TooSlow`] when either test fails.
pub fn speed_verdict(fps: f32, target_fps: u32, lat_ms: &[f32]) -> Result<(), TooSlow> {
    let target = target_fps.max(1);
    if fps < f32_from_u32(target) * 0.9 {
        return Err(TooSlow::Fps { got: fps, need: target });
    }
    let q = lat_ms.len() / 4;
    if q == 0 {
        return Ok(());
    }
    let (from_ms, to_ms) = (median(&lat_ms[..q]), median(&lat_ms[lat_ms.len() - q..]));
    let budget = 1000.0 / f32_from_u32(target);
    if to_ms - from_ms > 2.0 * budget {
        return Err(TooSlow::FallingBehind { from_ms, to_ms });
    }
    Ok(())
}

// ---------------------------------------------------------------- cache

fn cache_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::local_dir()?.join("encoders.json"))
}

fn load_cache() -> Value {
    cache_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}))
}

fn save_cache(v: &Value) {
    let Some(p) = cache_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Serializing a `Value` cannot fail; the cache is best effort anyway.
    if let Ok(bytes) = serde_json::to_vec_pretty(v) {
        let _ = std::fs::write(p, bytes);
    }
}

/// Cache key: codec, size, rate, monitor, capture adapters, ffmpeg build.
pub fn cache_key(t: &Tools, codec: Codec, w: u32, h: u32, fps: u32, device: &str, caps: &[Capture]) -> String {
    let ff = t
        .ffmpeg_src
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| {
            format!(
                "{}-{:?}",
                m.len(),
                m.modified().ok().map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            )
        })
        .unwrap_or_default();
    let ad: Vec<String> = caps.iter().map(|c| format!("{}:{}", c.pref.name(), c.adapter)).collect();
    format!("{}|{w}x{h}@{fps}|{device}|{}|{ff}", codec.name(), ad.join(","))
}

fn cached(key: &str) -> Option<(Backend, Path, String)> {
    let v = load_cache();
    let e = v.get(key)?;
    let b = Backend::parse(e.get("backend")?.as_str()?)?;
    let p = match e.get("path")?.as_str()? {
        "zero-copy" => Path::ZeroCopy,
        "copy" => Path::Copy,
        "copy+scale_cuda" => Path::CopyScaleCuda,
        _ => return None,
    };
    Some((b, p, e.get("pref")?.as_str()?.to_string()))
}

pub fn remember(key: &str, p: &Plan) {
    let mut v = load_cache();
    v[key] =
        json!({"backend": p.backend.name(), "path": p.path.name(), "pref": p.cap.pref.name(), "encoder": p.encoder()});
    save_cache(&v);
}

pub fn forget(key: &str) {
    let mut v = load_cache();
    if let Some(m) = v.as_object_mut() {
        m.remove(key);
    }
    save_cache(&v);
}

// ---------------------------------------------------------------- capabilities (`caps`)

/// One (encoder, codec) result of `tunnelrez-send caps`: can it really run at the size and rate?
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapResult {
    pub ok: bool,
    /// Why not (empty when `ok`).
    pub reason: String,
    /// Read from the on-disk cache instead of probed now.
    pub cached: bool,
}

/// The `caps` encoder name of an ffmpeg backend: `cpu` for the software encoders, else the backend name.
pub fn caps_name(b: Backend) -> &'static str {
    if b.software() {
        "cpu"
    } else {
        b.name()
    }
}

/// `caps` measured `b` for cache key `key` and it failed (logged): Auto must not pick it, cached or not.
fn caps_ruled_out(b: Backend, key: &str) -> bool {
    match caps_cached(&caps_key(caps_name(b), key)) {
        Some(r) if !r.ok => {
            eprintln!("[encoder] {} skipped (caps: {})", b.name(), r.reason);
            true
        }
        _ => false,
    }
}

/// The cache key of a `caps` result: the encoder name plus [`cache_key`]. `caps3`: results cached before native NVENC
/// was measured on every monitor held a false "not rendered by an NVIDIA GPU" failure (and before `caps2`, false "too
/// slow" ones, see [`speed_verdict`]), so they are measured again.
pub fn caps_key(encoder: &str, key: &str) -> String {
    format!("caps3|{encoder}|{key}")
}

/// The `caps` / probe reason for AMF on a PC without the AMD runtime.
pub const AMF_MISSING: &str = "no AMD GPU/driver (amfrt64.dll not found)";

/// Why `b` cannot run on this PC, when that is known without starting ffmpeg: AMF needs the AMD driver's
/// `amfrt64.dll` (ffmpeg fails late and confusingly without it, e.g. on an AV1 option).
pub fn backend_unavailable(b: Backend) -> Option<&'static str> {
    (b == Backend::Amf && !amf_runtime_present()).then_some(AMF_MISSING)
}

/// The AMD driver installs `amfrt64.dll` in `%SystemRoot%\System32`.
fn amf_runtime_present() -> bool {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    amf_runtime_in(&std::path::Path::new(&root).join("System32"))
}

fn amf_runtime_in(dir: &std::path::Path) -> bool {
    dir.join("amfrt64.dll").is_file()
}

/// A cached `caps` result.
pub fn caps_cached(key: &str) -> Option<CapResult> {
    let v = load_cache();
    let e = v.get(key)?;
    Some(CapResult { ok: e.get("ok")?.as_bool()?, reason: e.get("reason")?.as_str()?.to_string(), cached: true })
}

/// Store a `caps` result.
pub fn caps_remember(key: &str, r: &CapResult) {
    let mut v = load_cache();
    v[key] = json!({"ok": r.ok, "reason": r.reason});
    save_cache(&v);
}

/// The first line of a probe failure, at most 160 characters: short enough for a tooltip.
pub fn short_reason(r: &str) -> String {
    let line = r.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    line.chars().take(160).collect()
}

/// Can ffmpeg backend `backend` run a real stream: a measuring probe (45 frames after 15 warm-up, so the
/// encoder must keep up with the frame rate) on each capture/transfer candidate until one works.
pub fn backend_works(
    tools: &Tools,
    backend: Backend,
    caps: &[Capture],
    w: u32,
    h: u32,
    make: &dyn Fn(Backend, &Capture, Path) -> Plan,
    verbosity: Verbosity,
) -> CapResult {
    let mut last = String::from("no capture view on this GPU");
    for (cap, path) in encoder::candidates(backend, caps, w, h) {
        let probe = run(tools, &make(backend, &cap, path), 45, 15, IdrTest::Skip, verbosity);
        if probe.ok {
            return CapResult { ok: true, reason: String::new(), cached: false };
        }
        last = short_reason(&probe.reason);
    }
    CapResult { ok: false, reason: last, cached: false }
}

// ---------------------------------------------------------------- auto selection

/// Pick a working plan: the first backend in `order` (and its first working capture/transfer
/// candidate) that passes the probe. `make` fills in a plan for (backend, capture, path).
/// `banned` holds (backend, path) pairs that failed at runtime.
#[expect(clippy::too_many_arguments, reason = "internal helper; the arguments are independent probe inputs")]
pub fn choose<S: BuildHasher>(
    t: &Tools,
    order: &[Backend],
    caps: &[Capture],
    w: u32,
    h: u32,
    key: Option<&str>,
    banned: &HashSet<(Backend, Path), S>,
    make: &dyn Fn(Backend, &Capture, Path) -> Plan,
    verbosity: Verbosity,
) -> Option<Plan> {
    if let Some(k) = key {
        if let Some((b, p, pref)) = cached(k) {
            if order.contains(&b)
                && !banned.contains(&(b, p))
                && backend_unavailable(b).is_none()
                && !caps_ruled_out(b, k)
            {
                if let Some((c, _)) =
                    encoder::candidates(b, caps, w, h).into_iter().find(|(c, cp)| *cp == p && c.pref.name() == pref)
                {
                    let plan = make(b, &c, p);
                    eprintln!("[encoder] cached choice: {} (--reprobe to probe again)", plan.describe());
                    return Some(plan);
                }
            }
        }
    }
    let mut dead_views = HashSet::new();
    // Test knob: TUNNELREZ_DISABLE=nvenc,qsv makes those backends fail their probe (simulates a
    // PC without that hardware, e.g. to check the CPU fallback and its message).
    let disabled: Vec<Backend> = std::env::var("TUNNELREZ_DISABLE")
        .map(|v| v.split(',').filter_map(Backend::parse).collect())
        .unwrap_or_default();
    for &b in order {
        if disabled.contains(&b) {
            eprintln!("[encoder] probe failed: {} (disabled by TUNNELREZ_DISABLE)", b.name());
            continue;
        }
        if let Some(why) = backend_unavailable(b) {
            eprintln!("[encoder] probe failed: {} ({why})", b.name());
            continue;
        }
        // `caps` already measured this backend at this size and rate and it could not keep up: skip it.
        if key.is_some_and(|k| caps_ruled_out(b, k)) {
            continue;
        }
        for (c, p) in encoder::candidates(b, caps, w, h) {
            if banned.contains(&(b, p)) || dead_views.contains(&c.pref) {
                continue;
            }
            let plan = make(b, &c, p);
            // Software encoders must also show that they keep up.
            let r = if b.software() {
                run(t, &plan, 45, 15, IdrTest::Skip, verbosity)
            } else {
                run(t, &plan, QUICK_FRAMES, 0, IdrTest::Skip, verbosity)
            };
            if r.ok {
                eprintln!("[encoder] probe ok: {} (first frame after {:.0} ms)", plan.describe(), r.startup_ms);
                if b.software() && order.iter().any(|o| !o.software()) {
                    eprintln!(
                        "[encoder] no hardware encoder works on this PC; using the CPU encoder {}                          ({} threads, ~{:.1} cores at the probe). Run `tunnelrez-send doctor` to see why.",
                        plan.encoder(),
                        plan.threads,
                        r.cpu_cores
                    );
                }
                if let Some(k) = key {
                    remember(k, &plan);
                }
                return Some(plan);
            }
            if capture_failed(&r.reason) {
                dead_views.insert(c.pref);
            }
            eprintln!(
                "[encoder] probe failed: {} {} {} {}: {}",
                b.name(),
                plan.encoder(),
                c.pref.name(),
                p.name(),
                r.reason
            );
        }
    }
    None
}

pub type DeadViews = RefCell<HashMap<GpuPref, String>>;

/// The `doctor` table for one codec: every backend x candidate, measured.
#[expect(clippy::too_many_arguments, reason = "internal helper; the arguments are independent probe inputs")]
pub fn doctor_rows(
    t: &Tools,
    codec: Codec,
    caps: &[Capture],
    w: u32,
    h: u32,
    make: &dyn Fn(Backend, &Capture, Path) -> Plan,
    dead: &DeadViews,
    only: Option<Backend>,
    verbosity: Verbosity,
) {
    for b in encoder::ALL {
        if only.is_some_and(|o| o != b) {
            continue;
        }
        let Some(enc) = b.encoder(codec) else { continue };
        if let Some(why) = backend_unavailable(b) {
            println!("{:<7} {:<5} {:<11} {:<16} {:<5} {why}", b.name(), codec.name(), enc, "-", "FAIL");
            continue;
        }
        let cands = encoder::candidates(b, caps, w, h);
        if cands.is_empty() {
            println!(
                "{:<7} {:<5} {:<11} {:<16} {:<5} no capture view on this GPU",
                b.name(),
                codec.name(),
                enc,
                "-",
                "FAIL"
            );
            continue;
        }
        for (c, p) in cands {
            let path = format!("{} {}", c.pref.name(), p.name());
            if let Some(why) = dead.borrow().get(&c.pref) {
                println!(
                    "{:<7} {:<5} {:<11} {:<16} {:<5} capture fails on this view ({why})",
                    b.name(),
                    codec.name(),
                    enc,
                    path,
                    "FAIL"
                );
                continue;
            }
            let plan = make(b, &c, p);
            let r = run(t, &plan, MEASURE_FRAMES, WARMUP_FRAMES, IdrTest::Test, verbosity);
            if !r.ok && capture_failed(&r.reason) {
                dead.borrow_mut().insert(c.pref, r.reason.clone());
            }
            if r.ok {
                println!(
                    "{:<7} {:<5} {:<11} {:<16} {:<5} {:>6.1} {:>5.1} {:>5.1} {:>5.2} {:>8.0} {}",
                    b.name(),
                    codec.name(),
                    enc,
                    path,
                    "ok",
                    r.lat_p50,
                    r.lat_p99,
                    r.fps,
                    r.cpu_cores,
                    r.startup_ms,
                    match r.idr_ok {
                        Some(true) => "ok",
                        Some(false) => "NO",
                        None => "-",
                    }
                );
            } else {
                let extra = if r.frames > 0 { format!(" ({} frames)", r.frames) } else { String::new() };
                println!(
                    "{:<7} {:<5} {:<11} {:<16} {:<5} {}{}",
                    b.name(),
                    codec.name(),
                    enc,
                    path,
                    "FAIL",
                    r.reason,
                    extra
                );
            }
        }
    }
}

pub const DOCTOR_HEADER: &str =
    "backend codec encoder     capture/path     works lat_ms   p99   fps cores start_ms idr | or why not";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_names_and_reasons() {
        assert_eq!(caps_name(Backend::X265), "cpu");
        assert_eq!(caps_name(Backend::SvtAv1), "cpu");
        assert_eq!(caps_name(Backend::Nvenc), "nvenc");
        assert_eq!(caps_key("cpu", "hevc|3840x2160@60"), "caps3|cpu|hevc|3840x2160@60");
        assert_eq!(
            short_reason(
                "
  too slow: 40 fps
more"
            ),
            "too slow: 40 fps"
        );
        assert_eq!(short_reason(&"x".repeat(500)).len(), 160);
    }

    #[test]
    fn throughput_decides_not_latency() {
        // Intel QSV at 1080p60 on the user's PC: ~60 fps out, ~110 ms steady latency (pipelined). Must pass.
        assert_eq!(speed_verdict(60.0, 60, &[112.0; 90]), Ok(()));
        // High latency that falls (start-up backlog draining) passes too.
        let draining: Vec<f32> = (0..90u8).map(|i| 150.0 - f32::from(i)).collect();
        assert_eq!(speed_verdict(59.0, 60, &draining), Ok(()));
        // 54 fps is 90 % of 60: passes; 40 fps fails.
        assert_eq!(speed_verdict(54.0, 60, &[20.0; 90]), Ok(()));
        let slow = speed_verdict(40.0, 60, &[20.0; 90]).unwrap_err();
        assert_eq!(slow, TooSlow::Fps { got: 40.0, need: 60 });
        assert_eq!(slow.to_string(), "too slow: 40 fps, needs 60");
        // Few samples: no drift test.
        assert_eq!(speed_verdict(60.0, 60, &[10.0, 500.0]), Ok(()));
    }

    #[test]
    fn growing_latency_is_falling_behind() {
        // 35 ms -> 120 ms across the measured frames: more than 2 frame periods (33 ms at 60 fps).
        let mut lat = vec![35.0; 20];
        lat.extend([60.0; 50]);
        lat.extend([120.0; 20]);
        let slow = speed_verdict(58.0, 60, &lat).unwrap_err();
        assert_eq!(slow, TooSlow::FallingBehind { from_ms: 35.0, to_ms: 120.0 });
        assert_eq!(slow.to_string(), "too slow: falling behind, latency 35 -> 120 ms");
        assert!(short_reason(&slow.to_string()).starts_with("too slow: falling behind,"));
        // A rise within 2 frame periods passes (60 fps: 33.3 ms; 30 fps: 66.7 ms).
        let mut lat = vec![35.0; 45];
        lat.extend([65.0; 45]);
        assert_eq!(speed_verdict(60.0, 60, &lat), Ok(()));
        let mut lat = vec![35.0; 45];
        lat.extend([95.0; 45]);
        assert_eq!(speed_verdict(30.0, 30, &lat), Ok(()));
    }

    #[test]
    fn amf_needs_its_runtime() {
        let dir = std::env::temp_dir().join(format!("tunnelrez-amf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!amf_runtime_in(&dir));
        std::fs::write(dir.join("amfrt64.dll"), b"").unwrap();
        assert!(amf_runtime_in(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(backend_unavailable(Backend::Nvenc), None);
        assert_eq!(backend_unavailable(Backend::Amf).is_some(), !amf_runtime_present());
    }

    #[test]
    fn mft_missing_lines() {
        use crate::ffmpeg::is_mft_missing;
        assert!(is_mft_missing("[hevc_mf @ 000001] could not find any MFT for the given media type"));
        assert!(is_mft_missing("MFTEnumEx failed: 0xC00D5212"));
        assert!(!is_mft_missing("Error while opening encoder"));
    }
}
