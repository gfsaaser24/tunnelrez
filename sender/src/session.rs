//! One TCP session with the TV: HELLO, then CONFIG + FRAMEs from a [`Source`], PING/PONG clock
//! estimate, the TV's STATS, IDR requests. Shared by every encoder backend.

use crate::audio::sink::SinkChoice;
use crate::audio::stream::{self as audio, AudioMode, Link};
use crate::clock;
use crate::encoder::RateMode;
use crate::flv::{self, Codec};
use crate::level;
use crate::num::{f32_from_f64, f32_from_u128, f32_from_u64, f32_from_usize, f64_from_i64};
use crate::proto::{
    build, read_msg, FLAG_KEY, T_BYE, T_CAPS, T_CONFIG, T_FRAME, T_HELLO, T_IDR_REQUEST, T_PING, T_PONG, T_STATS,
};
use crate::source::{Au, Event, Source, Stages, StopMode};
use crate::stats::{pct, r1, ClockEst};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub static STOP: AtomicBool = AtomicBool::new(false);

/// Consecutive encoder failures before the session gives up on the encoder (`--encoder auto`
/// then demotes to the next backend).
pub const MAX_ENCODER_FAILS: u32 = 3;

#[derive(Clone, Debug)]
pub struct Cfg {
    pub codec: Codec,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    /// Shown in the stats line as `codec_out`, e.g. "hevc/nvenc copy".
    pub label: String,
    /// The encoder's target bitrate (stats line `target_mbps`).
    pub target_bps: u64,
    /// The `--bitrate` the user asked for; None = the codec's default.
    pub requested_bps: Option<u64>,
    /// Ceiling or fixed rate (stats line `fixed_rate`).
    pub rate: RateMode,
    pub verbose: bool,
    /// `--audio`.
    pub audio: AudioMode,
    /// `--audio-sink`.
    pub audio_sink: SinkChoice,
}

#[derive(Debug)]
pub enum End {
    Stop,
    Disconnected,
    /// The encoder failed `MAX_ENCODER_FAILS` times in a row.
    EncoderFailed(String),
}

#[derive(Default)]
struct SendStats {
    frames: u64,
    bytes: u64,
    keyframes: u64,
    enc_ms: Vec<f32>,    // capture -> frame fully read from the encoder
    cap2tx_ms: Vec<f32>, // capture -> written to socket
    tx_ms: Vec<f32>,     // socket write duration (backpressure)
    cap_iv_ms: Vec<f32>, // capture timestamp interval (cadence)
    cpu_cores: Option<f32>,
    idr_requests: u64,
    restarts: u64,
    last_restart_ms: Option<f32>,
    total_frames: u64,
    total_bytes: u64,
    /// Native backend stages (ms): present -> in process, colour conversion, wait for the encoder, NVENC.
    cap_ms: Vec<f32>,
    csc_ms: Vec<f32>,
    wait_ms: Vec<f32>,
    nvenc_ms: Vec<f32>,
}

struct Shared {
    w: Mutex<TcpStream>,
    dead: AtomicBool,
    idr: AtomicBool,
    idr_req_at: AtomicU64,
    clock: Mutex<ClockEst>,
    stats: Mutex<SendStats>,
    tv: Mutex<Option<(Instant, Value)>>,
    audio: Link,
}

/// Lock a session mutex. A poisoned lock means another session thread panicked: fatal.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().expect("session lock poisoned")
}

impl Shared {
    fn new(sock: TcpStream) -> Self {
        Shared {
            w: Mutex::new(sock),
            dead: AtomicBool::new(false),
            idr: AtomicBool::new(false),
            idr_req_at: AtomicU64::new(0),
            clock: Mutex::new(ClockEst::default()),
            stats: Mutex::new(SendStats::default()),
            tv: Mutex::new(None),
            audio: Link::default(),
        }
    }

    fn send(&self, buf: &[u8]) -> bool {
        let r = lock(&self.w).write_all(buf);
        if r.is_err() {
            self.dead.store(true, SeqCst);
        }
        r.is_ok()
    }
}

/// Connect to the TV (3 s timeout), `TCP_NODELAY`, 2 MiB send buffer.
///
/// # Errors
///
/// When the socket cannot be created or the connection fails or times out.
pub fn connect(addr: SocketAddr) -> Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    let _ = s.set_send_buffer_size(2 * 1024 * 1024);
    s.set_tcp_nodelay(true)?;
    s.connect_timeout(&addr.into(), Duration::from_secs(3))?;
    Ok(s.into())
}

fn reader_thread(sh: &Shared, mut r: TcpStream) {
    loop {
        let (h, p) = match read_msg(&mut r) {
            Ok(x) => x,
            Err(e) => {
                if !sh.dead.swap(true, SeqCst) && !STOP.load(SeqCst) {
                    eprintln!("[net] connection closed: {e}");
                }
                return;
            }
        };
        let now = clock::now_us();
        match h.typ {
            T_PONG if p.len() >= 16 => {
                if let (Some(t0), Some(tr)) = (p.first_chunk::<8>(), p[8..].first_chunk::<8>()) {
                    lock(&sh.clock).add(u64::from_le_bytes(*t0), u64::from_le_bytes(*tr), now);
                }
            }
            T_STATS => {
                if let Ok(v) = serde_json::from_slice::<Value>(&p) {
                    *lock(&sh.tv) = Some((Instant::now(), v));
                }
            }
            T_CAPS => {
                eprintln!("{}", tv_caps_line(&p));
                if let Ok(v) = serde_json::from_slice::<Value>(&p) {
                    sh.audio.on_caps(&v);
                }
            }
            T_IDR_REQUEST => {
                lock(&sh.stats).idr_requests += 1;
                // Store the request time before raising the flag, so the reader of the flag never
                // sees a missing or newer time.
                if !sh.idr.load(SeqCst) {
                    sh.idr_req_at.store(now, SeqCst);
                }
                sh.idr.store(true, SeqCst);
            }
            T_BYE => {
                sh.dead.store(true, SeqCst);
                return;
            }
            _ => {} // unknown: payload already consumed
        }
    }
}

/// The stderr line for one CAPS message: `[tv] caps <compact JSON>` (the app reads lines starting with
/// `[tv] caps {`), or `[tv] caps unreadable: <error>`.
fn tv_caps_line(payload: &[u8]) -> String {
    match serde_json::from_slice::<Value>(payload) {
        Ok(v) => format!("[tv] caps {v}"),
        Err(e) => format!("[tv] caps unreadable: {e}"),
    }
}

fn ping_thread(sh: &Shared) {
    while !sh.dead.load(SeqCst) && !STOP.load(SeqCst) {
        let t = clock::now_us();
        if !sh.send(&build(T_PING, 0, t, &[&t.to_le_bytes()])) {
            return;
        }
        for _ in 0..10 {
            if sh.dead.load(SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn stats_thread(sh: &Shared, c: &Cfg) {
    let size = format!("{}x{}@{}", c.w, c.h, c.fps);
    let mut last = Instant::now();
    while !sh.dead.load(SeqCst) && !STOP.load(SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        let el = last.elapsed();
        if el < Duration::from_secs(1) {
            continue;
        }
        last = Instant::now();
        let secs = el.as_secs_f32();
        let mut s = std::mem::take(&mut *lock(&sh.stats));
        {
            // keep cumulative counters
            let mut g = lock(&sh.stats);
            g.idr_requests = s.idr_requests;
            g.restarts = s.restarts;
            g.last_restart_ms = s.last_restart_ms;
            g.total_frames = s.total_frames;
            g.total_bytes = s.total_bytes;
        }
        let (e50, e99, _) = pct(&mut s.enc_ms);
        let (c50, c99, _) = pct(&mut s.cap2tx_ms);
        let (_, t99, tmax) = pct(&mut s.tx_ms);
        let (i50, i99, imax) = pct(&mut s.cap_iv_ms);
        let ck = lock(&sh.clock);
        let best = ck.best();
        let rtt_ms = best.map(|b| f32_from_u64(b.0) / 1000.0);
        let last_rtt = ck.last_rtt().map(|r| f32_from_u64(r) / 1000.0);
        drop(ck);

        let mut out = Map::new();
        let tv = lock(&sh.tv).clone();
        let tv = tv.filter(|(t, _)| t.elapsed() < Duration::from_secs(3)).map(|x| x.1);
        if let Some(Value::Object(m)) = &tv {
            for (k, v) in m {
                out.insert(k.clone(), v.clone());
            }
        }
        let f = |k: &str| tv.as_ref().and_then(|v| v.get(k)).and_then(serde_json::Value::as_f64);
        // End-to-end latency: exact if the TV reports when it displayed which frame (in its clock),
        // otherwise an estimate from sender-side capture->send + one-way network + TV-side time.
        let (e2e, src) = if let (Some(t), Some(d), Some((_, off))) = (f("disp_t_us"), f("disp_us"), best) {
            (Some(f32_from_f64(d - f64_from_i64(off) - t) / 1000.0), "tv_clock")
        } else {
            let net = rtt_ms.unwrap_or(0.0) / 2.0;
            if let Some(l) = f("lat_ms_p50") {
                (Some(c50 + net + f32_from_f64(l)), "est:cap2tx+rtt/2+tv_lat")
            } else if let Some(d) = f("decode_ms_p50") {
                (Some(c50 + net + f32_from_f64(d)), "est:cap2tx+rtt/2+decode")
            } else {
                (Some(c50 + net), "est:cap2tx+rtt/2")
            }
        };
        out.insert("enc_fps".into(), json!(r1(f32_from_u64(s.frames) / secs)));
        out.insert("send_mbps".into(), json!(r1(f32_from_u64(s.bytes) * 8.0 / 1e6 / secs)));
        out.insert("cap_iv_ms".into(), json!([r1(i50), r1(i99), r1(imax)]));
        out.insert("enc_ms_p50".into(), json!(r1(e50)));
        out.insert("enc_ms_p99".into(), json!(r1(e99)));
        out.insert("cap2tx_ms_p50".into(), json!(r1(c50)));
        out.insert("cap2tx_ms_p99".into(), json!(r1(c99)));
        out.insert("tx_ms_p99".into(), json!(r1(t99)));
        out.insert("tx_ms_max".into(), json!(r1(tmax)));
        out.insert("rtt_ms".into(), json!(rtt_ms.map(r1)));
        out.insert("rtt_ms_last".into(), json!(last_rtt.map(r1)));
        out.insert("e2e_ms".into(), json!(e2e.map(r1)));
        out.insert("e2e_src".into(), json!(src));
        out.insert("kf".into(), json!(s.keyframes));
        out.insert("idr_req".into(), json!(s.idr_requests));
        out.insert("restarts".into(), json!(s.restarts));
        if let Some(r) = s.last_restart_ms {
            out.insert("idr_ms".into(), json!(r1(r)));
        }
        native_stages(&mut out, &mut s, secs);
        if let Some(c) = s.cpu_cores {
            out.insert("enc_cpu_cores".into(), json!(r1(c)));
        }
        out.insert("codec_out".into(), json!(c.label));
        // The size really encoded and sent (the app shows this, not the picked one).
        out.insert("send_size".into(), json!(size));
        // The rate the encoder was asked for, next to `send_mbps` (what really went out).
        out.insert("target_mbps".into(), json!(r1(f32_from_u64(c.target_bps) / 1e6)));
        out.insert("fixed_rate".into(), json!(c.rate == RateMode::Fixed));
        audio_stats(&mut out, &sh.audio, secs, best.map(|b| b.1), (src == "tv_clock").then_some(e2e).flatten());
        println!("{}", Value::Object(out));
    }
}

/// The native backend's stage times (`[p50, p99]` ms) and `new_fps` (new pictures per second; the rest of
/// `enc_fps` re-encodes the last picture of a static screen). Capture, convert and wait are only timed on new
/// pictures, so a second with none has them `null`, not 0: a static screen must not pull the medians to 0.
/// Capture is also `null` when no new picture had a real capture time (see [`push_stages`]).
fn native_stages(out: &mut Map<String, Value>, s: &mut SendStats, secs: f32) {
    if s.nvenc_ms.is_empty() {
        return;
    }
    // Every new picture has a convert time; not every one has a capture time.
    out.insert("new_fps".into(), json!(r1(f32_from_usize(s.csc_ms.len()) / secs)));
    for (k, v) in [
        ("cap_ms", &mut s.cap_ms),
        ("csc_ms", &mut s.csc_ms),
        ("wait_ms", &mut s.wait_ms),
        ("nvenc_ms", &mut s.nvenc_ms),
    ] {
        let stage = if v.is_empty() {
            Value::Null
        } else {
            let (p50, p99, _) = pct(v);
            json!([r1(p50), r1(p99)])
        };
        out.insert(k.into(), stage);
    }
}

/// The audio keys of the stats line (PROTOCOL.md, "Audio"): the worker's state and counters, and the audio
/// end-to-end latency from the TV's `a_play_t_us` / `a_play_us` (`off`: the clock offset; `e2e`: the exact video
/// end-to-end latency, for `av_ms`).
fn audio_stats(out: &mut Map<String, Value>, link: &Link, secs: f32, off: Option<i64>, e2e: Option<f32>) {
    let a = link.take_stats();
    out.insert("audio".into(), json!(a.state));
    out.insert("audio_tv_only".into(), json!(a.tv_only));
    if a.state == "off" {
        return;
    }
    if !a.route.is_empty() {
        out.insert("audio_dev".into(), json!(a.device));
        out.insert("audio_route".into(), json!(a.route));
        out.insert("audio_pc".into(), json!(a.pc_echo));
    }
    out.insert("a_sent_s".into(), json!(r1(f32_from_u64(a.sent) / secs)));
    out.insert("a_cap_gaps".into(), json!(a.cap_gaps));
    let f = |k: &str| out.get(k).and_then(Value::as_f64);
    if let (Some(t), Some(d), Some(off)) = (f("a_play_t_us"), f("a_play_us"), off) {
        let a_e2e = f32_from_f64(d - f64_from_i64(off) - t) / 1000.0;
        out.insert("audio_e2e_ms".into(), json!(r1(a_e2e)));
        if let Some(v) = e2e {
            out.insert("av_ms".into(), json!(r1(a_e2e - v)));
        }
    }
}

/// The TV's address for the audio datagrams. `peer_addr` was seen failing right after a connect under load (the
/// audio thread then never started, silently), so it is retried briefly, and a failure is logged.
fn tv_addr(sock: &TcpStream) -> Option<SocketAddr> {
    let mut last = None;
    for _ in 0..20 {
        match sock.peer_addr() {
            Ok(a) => return Some(a),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(e) = last {
        eprintln!("[audio] cannot read the TV's address ({e}); no audio this session");
    }
    None
}

/// Start the reader, ping, stats and audio threads of one session.
fn spawn_threads(sh: &Arc<Shared>, reader: TcpStream, c: &Cfg) -> Vec<JoinHandle<()>> {
    let tv = tv_addr(&reader);
    vec![
        {
            let sh = Arc::clone(sh);
            std::thread::spawn(move || reader_thread(&sh, reader))
        },
        {
            let sh = Arc::clone(sh);
            std::thread::spawn(move || ping_thread(&sh))
        },
        {
            let (sh, c) = (Arc::clone(sh), c.clone());
            std::thread::spawn(move || stats_thread(&sh, &c))
        },
        {
            let (sh, mode) = (Arc::clone(sh), if tv.is_some() { c.audio } else { AudioMode::Off });
            let sink = c.audio_sink.clone();
            std::thread::spawn(move || {
                let Some(tv) = tv else {
                    sh.audio.set_error();
                    return;
                };
                let send = |b: &[u8]| sh.send(b);
                let done = || sh.dead.load(SeqCst) || STOP.load(SeqCst);
                audio::run(mode, &audio::Session { link: &sh.audio, tv, send_tcp: &send, done: &done, sink: &sink });
            })
        },
    ]
}

/// Stats bookkeeping for one encoder run.
struct RunStats {
    last_cap: Option<u64>,
    cpu_mark: Option<(Instant, u64)>,
}

/// One FRAME (plus CONFIG for a keyframe) that went out.
struct Sent {
    /// Its `t_us` (capture time, made monotonic).
    cap: u64,
    /// Socket write start and end.
    t0: u64,
    t1: u64,
    /// Bytes written.
    len: u64,
    key: bool,
}

/// Record one sent frame in the stats; a keyframe also settles a pending IDR request.
fn record_frame(
    sh: &Shared,
    src: &dyn Source,
    rs: &mut RunStats,
    au: &Au,
    f: &Sent,
    restart_from: &mut Option<u64>,
    c: &Cfg,
) {
    let mut s = lock(&sh.stats);
    s.frames += 1;
    s.total_frames += 1;
    s.bytes += f.len;
    s.total_bytes += f.len;
    s.enc_ms.push(f32_from_u64(au.arrive_us.saturating_sub(f.cap)) / 1000.0);
    s.cap2tx_ms.push(f32_from_u64(f.t1.saturating_sub(f.cap)) / 1000.0);
    s.tx_ms.push(f32_from_u64(f.t1 - f.t0) / 1000.0);
    if let Some(l) = rs.last_cap {
        s.cap_iv_ms.push(f32_from_u64(f.cap.saturating_sub(l)) / 1000.0);
    }
    rs.last_cap = Some(f.cap);
    if let Some(g) = au.stages {
        push_stages(&mut s, au, &g, f.cap);
    }
    if let Some((t, c0)) = rs.cpu_mark {
        if t.elapsed() >= Duration::from_secs(1) {
            if let Some(c1) = src.cpu_time_us() {
                s.cpu_cores = Some(f32_from_u64(c1.saturating_sub(c0)) / f32_from_u128(t.elapsed().as_micros()));
                rs.cpu_mark = Some((Instant::now(), c1));
            }
        }
    }
    if f.key {
        s.keyframes += 1;
        // A keyframe satisfies any pending IDR request.
        if sh.idr.swap(false, SeqCst) && restart_from.is_none() {
            *restart_from = Some(sh.idr_req_at.load(SeqCst));
        }
        if let Some(r) = restart_from.take() {
            s.last_restart_ms = Some(f32_from_u64(f.t1.saturating_sub(r)) / 1000.0);
            if c.verbose {
                eprintln!("[idr] request -> keyframe sent in {:.1} ms", f32_from_u64(f.t1.saturating_sub(r)) / 1000.0);
            }
        }
    }
}

/// The native backend's stage samples of one AU (`cap`: its sent `t_us`). The native crate sets the capture time
/// to `min(api_frame_time, arrive)`: when the API time is not before pick-up (WGC), it is clamped to the pick-up
/// time and the capture stage is unknown, so that frame adds no `cap_ms` sample (not a fake 0).
fn push_stages(s: &mut SendStats, au: &Au, g: &Stages, cap: u64) {
    let ms = |a: u64, b: u64| f32_from_u64(a.saturating_sub(b)) / 1000.0;
    s.nvenc_ms.push(ms(au.arrive_us, g.encode_start_us));
    if !g.repeat {
        if au.cap_us < g.arrive_us {
            s.cap_ms.push(ms(g.arrive_us, cap));
        }
        s.csc_ms.push(ms(g.convert_done_us, g.arrive_us));
        s.wait_ms.push(ms(g.encode_start_us, g.convert_done_us));
    }
}

/// Handle a pending IDR request on a running encoder. True when the encoder must be restarted:
/// it cannot force an IDR, or no keyframe came 600 ms after the request.
fn idr_needs_restart(sh: &Shared, src: &mut dyn Source, restart_from: &mut Option<u64>) -> bool {
    if restart_from.is_none() && sh.idr.swap(false, SeqCst) {
        // Ask the running encoder for an IDR (CONFIG + keyframe follow); restart if that fails.
        *restart_from = Some(sh.idr_req_at.load(SeqCst));
        if !src.force_idr() {
            return true;
        }
    }
    if let Some(r) = *restart_from {
        if clock::now_us().saturating_sub(r) > 600_000 {
            eprintln!("[idr] no keyframe 600 ms after request; restarting the encoder");
            // The new encoder gets its own 600 ms, so a late first keyframe cannot loop restarts.
            *restart_from = Some(clock::now_us());
            return true;
        }
    }
    false
}

/// After one more encoder failure in a row (`fails`): give up with [`End::EncoderFailed`] at
/// [`MAX_ENCODER_FAILS`], else wait 300 ms before the restart.
fn give_up(fails: u32, why: String) -> Option<End> {
    if fails >= MAX_ENCODER_FAILS {
        return Some(End::EncoderFailed(why));
    }
    std::thread::sleep(Duration::from_millis(300));
    None
}

/// The encoder ended (`next` returned None/Err): stop it and count the failure.
fn on_encoder_end(
    mut src: Box<dyn Source>,
    started: u64,
    fails: &mut u32,
    good_since: &mut Option<Instant>,
) -> Option<End> {
    let why = src.exit_reason();
    let ran_ms = (clock::now_us() - started) / 1000;
    src.stop(StopMode::Kill);
    if good_since.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
        *fails = 0; // it ran fine for a while: start counting again
    }
    *good_since = None;
    *fails += 1;
    eprintln!("[encoder] ended after {ran_ms} ms ({fails}/{MAX_ENCODER_FAILS}): {why}");
    give_up(*fails, why)
}

/// The start line: the bitrate asked for, the bitrate the encoder was configured with, and the level its SPS
/// carries, so a run shows what really ran.
fn rate_line(c: &Cfg, sps: Option<level::SpsLevel>) -> String {
    let mb = |b: u64| f32_from_u64(b) / 1e6;
    let asked = c.requested_bps.map_or_else(|| "default".to_string(), |b| format!("{:.1} Mb/s", mb(b)));
    let lvl = sps.map_or_else(|| format!("{} level n/a", c.codec.name()), |l| l.to_string());
    format!("[encoder] bitrate requested {asked}, configured {:.1} Mb/s ({}), {lvl}", mb(c.target_bps), c.rate.name())
}

/// Run one session on a connected socket. `make` starts the encoder (again after a failure or
/// an IDR that did not arrive in time).
///
/// # Errors
///
/// When the socket cannot be cloned for the reader thread. Network and encoder failures are
/// not errors: they end the session with [`End::Disconnected`] / [`End::EncoderFailed`].
///
/// # Panics
///
/// When a session lock is poisoned (another session thread panicked).
pub fn run(
    sock: TcpStream,
    c: &Cfg,
    deadline: Option<Instant>,
    make: &mut dyn FnMut() -> Result<Box<dyn Source>>,
) -> Result<End> {
    let reader = sock.try_clone()?;
    let sh = Arc::new(Shared::new(sock));
    let hello = json!({"v":0,"codec":c.codec.name(),"width":c.w,"height":c.h,"fps":c.fps}).to_string();
    if !sh.send(&build(T_HELLO, 0, clock::now_us(), &[hello.as_bytes()])) {
        return Ok(End::Disconnected);
    }
    let th = spawn_threads(&sh, reader, c);

    let mut end = End::Disconnected;
    let mut fails = 0u32;
    let mut good_since: Option<Instant> = None;
    let mut restart_from: Option<u64> = None; // IDR request time, for restart latency
                                              // Protocol: t_us never goes backwards within a session. The native backend stamps a repeated
                                              // (static screen) picture with its re-encode time, so the next new picture can carry an earlier
                                              // capture time; it is sent 1 µs after the previous one instead.
    let mut last_sent_t: u64 = 0;
    'outer: loop {
        let mut src = match make() {
            Ok(s) => s,
            Err(e) => {
                fails += 1;
                eprintln!("[encoder] start failed ({fails}/{MAX_ENCODER_FAILS}): {e:#}");
                if let Some(e) = give_up(fails, format!("{e:#}")) {
                    end = e;
                    break 'outer;
                }
                continue 'outer;
            }
        };
        let started = clock::now_us();
        let mut config: Vec<u8> = Vec::new();
        let mut got_frame = false;
        let mut logged_rate = false;
        let mut rs = RunStats { last_cap: None, cpu_mark: src.cpu_time_us().map(|c| (Instant::now(), c)) };
        loop {
            if STOP.load(SeqCst) || deadline.is_some_and(|d| Instant::now() >= d) {
                end = End::Stop;
                sh.dead.store(true, SeqCst); // quiet the reader: the TV closing after BYE is expected
                                             // BYE first, then stop the encoder.
                sh.send(&build(T_BYE, 0, clock::now_us(), &[]));
                src.stop(StopMode::Graceful);
                break 'outer;
            }
            if sh.dead.load(SeqCst) {
                src.stop(StopMode::Kill);
                break 'outer;
            }
            if got_frame && idr_needs_restart(&sh, &mut *src, &mut restart_from) {
                src.stop(StopMode::Kill);
                lock(&sh.stats).restarts += 1;
                continue 'outer;
            }
            let Ok(Some(ev)) = src.next() else {
                if let Some(e) = on_encoder_end(src, started, &mut fails, &mut good_since) {
                    end = e;
                    break 'outer;
                }
                continue 'outer;
            };
            let au = match ev {
                Event::Config(cf) => {
                    config = cf;
                    continue;
                }
                Event::Frame(au) => au,
                Event::Idle => continue,
            };
            if !got_frame {
                good_since = Some(Instant::now());
            }
            got_frame = true;
            let key = au.key || is_key(c.codec, &au.data);
            let cap = monotonic_t(au.cap_us, &mut last_sent_t);
            let mut msg = Vec::with_capacity(au.data.len() + config.len() + 64);
            let kind = if key { FrameKind::Key } else { FrameKind::Delta };
            let frame = build_frame(c.codec, kind, &au.data, &mut config);
            if key {
                if !std::mem::replace(&mut logged_rate, true) {
                    eprintln!("{}", rate_line(c, level::sps_level(c.codec, &config)));
                }
                msg.extend_from_slice(&build(T_CONFIG, 0, cap, &[&config]));
            }
            msg.extend_from_slice(&build(T_FRAME, if key { FLAG_KEY } else { 0 }, cap, &[&frame]));
            let t0 = clock::now_us();
            if !sh.send(&msg) {
                src.stop(StopMode::Kill);
                break 'outer;
            }
            let t1 = clock::now_us();
            let sent = Sent { cap, t0, t1, len: u64::try_from(msg.len()).unwrap_or(u64::MAX), key };
            record_frame(&sh, &*src, &mut rs, &au, &sent, &mut restart_from, c);
        }
    }
    sh.dead.store(true, SeqCst);
    let _ = lock(&sh.w).shutdown(Shutdown::Both);
    for t in th {
        let _ = t.join();
    }
    let s = lock(&sh.stats);
    eprintln!(
        "[session] frames={} bytes={} idr_req={} restarts={}",
        s.total_frames, s.total_bytes, s.idr_requests, s.restarts
    );
    Ok(end)
}

/// `t` raised to at least `last + 1`; updates `last`.
fn monotonic_t(t: u64, last: &mut u64) -> u64 {
    let t = if *last == 0 { t } else { t.max(*last + 1) };
    *last = t;
    t
}

pub fn is_key(c: Codec, d: &[u8]) -> bool {
    match c {
        Codec::H264 => flv::annexb_nals(d).iter().any(|n| flv::nal_type(c, n) == 5),
        Codec::Hevc => flv::annexb_nals(d).iter().any(|n| (16..=21).contains(&flv::nal_type(c, n))),
        Codec::Av1 => false,
    }
}

/// Whether a FRAME is a keyframe (sent with `FLAG_KEY`, after a CONFIG).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameKind {
    Key,
    Delta,
}

/// Final FRAME payload. Keyframes carry their parameter sets / sequence header in-band too, and
/// AV1 frames get a temporal delimiter so each FRAME is a complete temporal unit.
pub fn build_frame(c: Codec, kind: FrameKind, data: &[u8], config: &mut Vec<u8>) -> Vec<u8> {
    let key = kind == FrameKind::Key;
    match c {
        Codec::H264 | Codec::Hevc => {
            if key && !flv::has_param_sets(c, data) && !config.is_empty() {
                [config.as_slice(), data].concat()
            } else {
                if key && config.is_empty() {
                    // take params from the AU itself
                    let ps: Vec<u8> = flv::annexb_nals(data)
                        .iter()
                        .filter(|n| matches!((c, flv::nal_type(c, n)), (Codec::H264, 7 | 8) | (Codec::Hevc, 32..=34)))
                        .flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat())
                        .collect();
                    *config = ps;
                }
                data.to_vec()
            }
        }
        Codec::Av1 => {
            let obus = flv::av1_obus(data);
            let has_td = obus.first().is_some_and(|o| o.0 == flv::AV1_OBU_TD);
            let seq = obus.iter().find(|o| o.0 == flv::AV1_OBU_SEQ_HDR).map(|o| o.1.to_vec());
            if let Some(s) = &seq {
                if config.is_empty() {
                    config.clone_from(s);
                }
            }
            let mut out = Vec::with_capacity(data.len() + config.len() + 2);
            let body = if has_td { &data[2..] } else { data };
            out.extend_from_slice(&flv::AV1_TD);
            if key && seq.is_none() {
                out.extend_from_slice(config);
            }
            out.extend_from_slice(body);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        level, monotonic_t, native_stages, push_stages, rate_line, tv_caps_line, Au, AudioMode, Cfg, Codec, RateMode,
        SendStats, Stages,
    };
    use serde_json::{json, Map};

    #[test]
    fn rate_line_shows_asked_configured_and_level() {
        let c = Cfg {
            codec: Codec::H264,
            w: 3840,
            h: 2160,
            fps: 60,
            label: "h264/native".into(),
            target_bps: 150_000_000,
            requested_bps: Some(150_000_000),
            rate: RateMode::Fixed,
            verbose: false,
            audio: AudioMode::On,
            audio_sink: crate::audio::sink::SinkChoice::Auto,
        };
        assert_eq!(
            rate_line(&c, Some(level::SpsLevel::H264(52))),
            "[encoder] bitrate requested 150.0 Mb/s, configured 150.0 Mb/s (fixed), H.264 level 5.2"
        );
        let d = Cfg { requested_bps: None, target_bps: 60_000_000, rate: RateMode::Ceiling, ..c };
        assert_eq!(
            rate_line(&d, None),
            "[encoder] bitrate requested default, configured 60.0 Mb/s (ceiling), h264 level n/a"
        );
    }

    #[test]
    fn native_stages_are_null_in_a_second_with_only_repeats() {
        // A static screen: every AU re-encodes the last picture, so only NVENC is timed.
        let mut s = SendStats { nvenc_ms: vec![8.0, 8.4], ..SendStats::default() };
        let mut out = Map::new();
        native_stages(&mut out, &mut s, 1.0);
        assert_eq!(out["new_fps"], json!(0.0));
        assert_eq!(out["nvenc_ms"], json!([8.4, 8.4]));
        for k in ["cap_ms", "csc_ms", "wait_ms"] {
            assert!(out[k].is_null(), "{k}: {}", out[k]);
        }
        // New pictures: real times.
        let mut s = SendStats {
            nvenc_ms: vec![2.0; 4],
            cap_ms: vec![5.0, 6.0],
            csc_ms: vec![0.3, 0.4],
            wait_ms: vec![0.1, 0.2],
            ..SendStats::default()
        };
        let mut out = Map::new();
        native_stages(&mut out, &mut s, 2.0);
        assert_eq!(out["new_fps"], json!(1.0));
        assert_eq!(out["cap_ms"], json!([6.0, 6.0]));
        assert_eq!(out["csc_ms"], json!([0.4, 0.4]));
        // FFmpeg (no stages): nothing.
        let mut out = Map::new();
        native_stages(&mut out, &mut SendStats::default(), 1.0);
        assert!(out.is_empty());
    }

    #[test]
    fn clamped_capture_time_gives_no_cap_sample() {
        let g =
            Stages { arrive_us: 10_000, convert_done_us: 10_400, xfer_us: 0, encode_start_us: 10_500, repeat: false };
        let au = |cap_us| Au { data: Vec::new(), cap_us, arrive_us: 12_500, key: false, stages: Some(g) };
        let mut s = SendStats::default();
        // WGC at 1080p: capture time clamped to the pick-up time, so the capture stage is unknown.
        push_stages(&mut s, &au(10_000), &g, 10_000);
        assert_eq!(s.cap_ms, Vec::<f32>::new());
        assert_eq!((s.csc_ms.len(), s.wait_ms.len(), s.nvenc_ms.len()), (1, 1, 1));
        let mut out = Map::new();
        native_stages(&mut out, &mut s, 1.0);
        assert!(out["cap_ms"].is_null(), "{}", out["cap_ms"]);
        assert_eq!(out["new_fps"], json!(1.0));
        assert_eq!(out["csc_ms"], json!([0.4, 0.4]));
        assert_eq!(out["nvenc_ms"], json!([2.0, 2.0]));
        // A real capture time before pick-up: timed (against the sent t_us).
        let mut s = SendStats::default();
        push_stages(&mut s, &au(4_000), &g, 4_000);
        assert_eq!(s.cap_ms, vec![6.0]);
    }

    #[test]
    fn tv_caps_line_is_compact_json() {
        let p = br#"{ "v": 1, "caps": [],
            "decoders": [ {"name": "OMX.MTK.VIDEO.DECODER.AV1", "mime": "video/av01", "max_w": 4096,
                           "max_h": 2304, "max_fps": 60, "max_bps": 60000000, "hw": true, "perf": ["3840x2160@60"]} ] }"#;
        let line = tv_caps_line(p);
        assert!(line.starts_with("[tv] caps {"), "{line}");
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line["[tv] caps ".len()..]).unwrap();
        assert_eq!(v["decoders"][0]["mime"], "video/av01");
        assert_eq!(v["decoders"][0]["perf"][0], "3840x2160@60");
        assert!(tv_caps_line(b"{nope").starts_with("[tv] caps unreadable: "));
    }

    #[test]
    fn t_us_never_goes_backwards() {
        let mut last = 0;
        assert_eq!(monotonic_t(100, &mut last), 100);
        assert_eq!(monotonic_t(97, &mut last), 101);
        assert_eq!(monotonic_t(200, &mut last), 200);
    }
}
