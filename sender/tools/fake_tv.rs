//! `fake_tv`: stand-in for the TV receiver, for testing tunnelrez-send without the TV.
//!
//! Listens on 127.0.0.1:7878 (one client at a time), validates the LRT0 framing and stream rules,
//! sends a CAPS message (its "decoders": AVC, HEVC and AV1, 4096x2304@60, hw) right after a client
//! connects, answers PING with PONG, sends a STATS JSON once per second (with `disp_t_us`/`disp_us`
//! so the sender's exact end-to-end path is exercised), and can dump the stream to a file.
//!
//!   `fake_tv` [--listen 127.0.0.1:7878] [--dump out.h264] [--idr-every SECS] [--once]
//!           [--no-caps | --caps-no-av1] [--no-audio] [--tone | --silent-tone]
//!
//! `--no-caps` sends no CAPS (like an older receiver); `--caps-no-av1` leaves the AV1 decoder out.
//!
//! Audio (PROTOCOL.md, "Audio"): CAPS lists `audio` (`--no-audio`: like a TV app without audio), AUDIO datagrams
//! are received on UDP at the listen address and played out by the TV's own playout code (`fake_tv/audio.rs`).
//! STATS gets the TV's `a_*` keys plus `a_net_ms` (capture -> arrival). `--tone` plays a quiet 440 Hz tone on
//! this PC from `AUDIO_START` on, so the loopback carries real audio: use it with the sender's `--audio tv-only`
//! (which mutes the PC before `AUDIO_START`), or it is audible.
//!
//! The dump is the raw elementary stream (CONFIG payloads + FRAME payloads, in order): Annex-B for
//! H.264/HEVC, OBU stream for AV1. A `<dump>.ts.csv` file gets one line per frame:
//! `t_us,recv_us,size,key`.

#[path = "fake_tv/audio.rs"]
mod audio;

use anyhow::Context;
use audio::Audio;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tunnelrez::clock::now_us;
use tunnelrez::num::{f32_from_i64, f32_from_u64, u64_sat_from_f32};
use tunnelrez::proto::{
    read_msg, type_name, write_msg, Header, FLAG_KEY, HDR_LEN, T_AUDIO_START, T_AUDIO_STOP, T_BYE, T_CAPS, T_CONFIG,
    T_FRAME, T_HELLO, T_IDR_REQUEST, T_PING, T_PONG, T_STATS,
};
use tunnelrez::stats::{pct, r1};

/// One STATS window (reset every second, except the last frame seen).
#[derive(Default)]
struct Win {
    frames: u64,
    bytes: u64,
    iv_ms: Vec<f32>,
    lat_ms: Vec<f32>, // (recv time - offset) - t_us using the true clock (same PC => offset 0)
    last_t: u64,
    last_recv: u64,
}

/// Lock a `fake_tv` mutex. A poisoned lock means the other thread panicked: fatal.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().expect("fake_tv lock poisoned")
}

/// A byte count as `u64` (lossless on every target we build).
fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn main() -> anyhow::Result<()> {
    let mut listen = "127.0.0.1:7878".to_string();
    let mut dump: Option<String> = None;
    let mut idr_every: Option<f64> = None;
    let mut once = false;
    let mut caps = Caps::All;
    let mut opts = AudioOpts { caps: true, tone: None };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--listen" => listen = it.next().context("--listen needs a value")?,
            "--dump" => dump = it.next(),
            "--idr-every" => idr_every = Some(it.next().context("--idr-every needs a value")?.parse()?),
            "--once" => once = true,
            "--no-caps" => caps = Caps::None,
            "--caps-no-av1" => caps = Caps::NoAv1,
            "--no-audio" => opts.caps = false,
            "--tone" => opts.tone = Some(0.05),
            // A stream of digital silence: loopback still delivers packets at the real engine timing, and
            // nothing can be heard on any device.
            "--silent-tone" => opts.tone = Some(0.0),
            o => anyhow::bail!("unknown arg {o}"),
        }
    }
    let listener = TcpListener::bind(&listen)?;
    let audio = Arc::new(Audio::default());
    audio::udp_thread(Arc::clone(&audio), &listen)?;
    eprintln!("[fake_tv] listening on {listen} (TCP, and UDP for audio)");
    for sock in listener.incoming() {
        let sock = sock?;
        let _ = sock.set_nodelay(true);
        eprintln!("[fake_tv] client {}", sock.peer_addr()?);
        if let Err(e) = session(sock, dump.as_deref(), idr_every, caps, &audio, opts) {
            eprintln!("[fake_tv] session error: {e}");
        }
        if once {
            break;
        }
    }
    Ok(())
}

/// `--no-audio` / `--tone` / `--silent-tone`.
#[derive(Clone, Copy, Debug)]
struct AudioOpts {
    /// CAPS lists `audio`.
    caps: bool,
    /// Play a test tone from `AUDIO_START` on, at this amplitude (0 = digital silence).
    tone: Option<f32>,
}

/// Which CAPS message `fake_tv` sends after a client connects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Caps {
    /// AVC, HEVC and AV1 decoders.
    All,
    /// AVC and HEVC only.
    NoAv1,
    /// No CAPS at all (an older receiver).
    None,
}

/// The CAPS payload (PROTOCOL.md, type 8), or None for [`Caps::None`].
fn caps_json(caps: Caps, opts: AudioOpts) -> Option<Value> {
    let mimes: &[(&str, &str)] = match caps {
        Caps::None => return None,
        Caps::NoAv1 => &[("AVC", "video/avc"), ("HEVC", "video/hevc")],
        Caps::All => &[("AVC", "video/avc"), ("HEVC", "video/hevc"), ("AV1", "video/av01")],
    };
    let decoders: Vec<Value> = mimes
        .iter()
        .map(|(name, mime)| {
            json!({
                "name": format!("OMX.FAKE.VIDEO.DECODER.{name}"), "mime": mime,
                "max_w": 4096, "max_h": 2304, "max_fps": 60, "max_bps": 60_000_000,
                "hw": true, "perf": ["3840x2160@60"],
            })
        })
        .collect();
    let flags: &[&str] = if opts.caps { &["audio"] } else { &[] };
    Some(json!({"v": 1, "caps": flags, "decoders": decoders}))
}

/// STATS once per second, plus an `IDR_REQUEST` every `idr_every` seconds, until `done`.
fn stats_loop(w: &Mutex<TcpStream>, done: &AtomicBool, win: &Mutex<Win>, idr_every: Option<f64>, audio: &Audio) {
    let mut last = Instant::now();
    let mut last_idr = Instant::now();
    while !done.load(SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
        if let Some(every) = idr_every {
            if last_idr.elapsed().as_secs_f64() >= every {
                last_idr = Instant::now();
                eprintln!("[fake_tv] sending IDR_REQUEST");
                let _ = write_msg(&mut *lock(w), T_IDR_REQUEST, 0, now_us(), &[]);
            }
        }
        let el = last.elapsed();
        if el < Duration::from_secs(1) {
            continue;
        }
        last = Instant::now();
        let secs = el.as_secs_f32();
        let mut g = std::mem::take(&mut *lock(win));
        {
            let mut keep = lock(win);
            keep.last_t = g.last_t;
            keep.last_recv = g.last_recv;
        }
        let (i50, i99, imax) = pct(&mut g.iv_ms);
        let (l50, l99, _) = pct(&mut g.lat_ms);
        let fps = f32_from_u64(g.frames) / secs;
        // The video "display" here is the receipt: audio syncs to that delay, like the TV to its display time.
        if g.lat_ms.is_empty() {
            audio.video_clear();
        } else {
            audio.video_sample(tunnelrez::num::i64_sat_from_f64(f64::from(l50) * 1000.0));
        }
        let mut js = json!({
            "fps_in": r1(fps), "fps_out": r1(fps), "dropped": 0,
            "decode_ms_p50": 0.0, "decode_ms_p99": 0.0, "queue": 0,
            "recv_kbps": u64_sat_from_f32((f32_from_u64(g.bytes) * 8.0 / 1000.0 / secs).round()),
            "late": 0, "codec": "fake",
            // extra (optional) fields: last frame shown, in sender clock / receiver clock
            "disp_t_us": g.last_t, "disp_us": g.last_recv,
            "lat_ms_p50": r1(l50), "lat_ms_p99": r1(l99),
            "interval_ms": [r1(i50), r1(i99), r1(imax)],
        });
        audio.stats(&mut js, secs);
        let js = js.to_string();
        if write_msg(&mut *lock(w), T_STATS, 0, now_us(), &[js.as_bytes()]).is_err() {
            return;
        }
    }
}

/// Optional dump files.
struct Sinks {
    /// Elementary stream.
    out: Option<BufWriter<File>>,
    /// `t_us,recv_us,size,key` per frame.
    ts: Option<BufWriter<File>>,
}

/// What the receive loop does after one message.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Bye,
}

/// Receive-side stream state, counters and samples for the summary.
struct Rx {
    n: u64,
    have_hello: bool,
    have_config: bool,
    want_key: bool,
    last_t: u64,
    last_recv: u64,
    errors: u64,
    kf: u64,
    frames: u64,
    bytes: u64,
    iv_all: Vec<f32>,
    recv_iv_all: Vec<f32>,
    lat_all: Vec<f32>,
    hello: Value,
}

impl Rx {
    fn new() -> Self {
        Rx {
            n: 0,
            have_hello: false,
            have_config: false,
            want_key: true,
            last_t: 0,
            last_recv: 0,
            errors: 0,
            kf: 0,
            frames: 0,
            bytes: 0,
            iv_all: Vec::new(),
            recv_iv_all: Vec::new(),
            lat_all: Vec::new(),
            hello: Value::Null,
        }
    }

    fn err(&mut self, m: &str) {
        self.errors += 1;
        eprintln!("[fake_tv] PROTOCOL ERROR: {m}");
    }

    /// Check and handle one message; Err only for dump-file or socket write failures.
    fn on_msg(
        &mut self,
        h: &Header,
        p: &[u8],
        w: &Mutex<TcpStream>,
        win: &Mutex<Win>,
        sinks: &mut Sinks,
        audio: (&Audio, IpAddr),
    ) -> anyhow::Result<Flow> {
        let now = now_us();
        self.n += 1;
        let idx = self.n;
        if h.reserved != 0 {
            self.err(&format!("msg {idx}: reserved = {}", h.reserved));
        }
        if !self.have_hello && h.typ != T_HELLO {
            self.err(&format!("msg {idx}: {} before HELLO", type_name(h.typ)));
        }
        match h.typ {
            T_HELLO => {
                self.hello = serde_json::from_slice(p).unwrap_or(Value::Null);
                eprintln!("[fake_tv] HELLO {}", self.hello);
                if self.hello.get("v").and_then(serde_json::Value::as_u64) != Some(0) {
                    self.err("HELLO v != 0");
                }
                self.have_hello = true;
            }
            T_CONFIG => {
                if p.is_empty() {
                    self.err("empty CONFIG");
                }
                self.have_config = true;
                // AV1 FRAMEs are complete temporal units (TD + sequence header on keyframes), and a
                // bare sequence header without a TD is not a valid OBU stream: dump only H.264/HEVC config.
                let av1 = self.hello.get("codec").and_then(|c| c.as_str()) == Some("av1");
                if let (Some(o), false) = (sinks.out.as_mut(), av1) {
                    o.write_all(p)?;
                }
            }
            T_FRAME => self.on_frame(h, p, now, win, sinks)?,
            T_PING => {
                if p.len() != 8 {
                    self.err(&format!("PING payload {} bytes", p.len()));
                }
                let echo = p.first_chunk::<8>().map_or(h.t_us, |b| u64::from_le_bytes(*b));
                let mut pl = echo.to_le_bytes().to_vec();
                pl.extend_from_slice(&now_us().to_le_bytes());
                write_msg(&mut *lock(w), T_PONG, 0, now_us(), &[&pl])?;
            }
            T_AUDIO_START => match audio.0.start(audio.1, p) {
                Ok(sid) => eprintln!("[fake_tv] AUDIO_START sid {sid:08x}: {}", String::from_utf8_lossy(p)),
                Err(e) => self.err(&format!("bad AUDIO_START: {e}")),
            },
            T_AUDIO_STOP => {
                eprintln!("[fake_tv] AUDIO_STOP");
                audio.0.stop();
            }
            T_BYE => {
                eprintln!("[fake_tv] BYE");
                return Ok(Flow::Bye);
            }
            other => self.err(&format!("unexpected type {other} from sender")),
        }
        if h.typ == T_CONFIG {
            self.want_key = true;
        }
        Ok(Flow::Continue)
    }

    fn on_frame(&mut self, h: &Header, p: &[u8], now: u64, win: &Mutex<Win>, sinks: &mut Sinks) -> anyhow::Result<()> {
        let key = h.flags & FLAG_KEY != 0;
        let frames = self.frames;
        if !self.have_config {
            self.err(&format!("FRAME {frames} before any CONFIG"));
        }
        if self.want_key && !key {
            self.err(&format!("FRAME {frames}: first frame after CONFIG is not a keyframe"));
        }
        self.want_key = false;
        if h.t_us < self.last_t {
            self.err(&format!("FRAME {frames}: t_us went backwards ({} < {})", h.t_us, self.last_t));
        }
        if self.last_t != 0 {
            let iv = f32_from_u64(h.t_us - self.last_t) / 1000.0;
            self.iv_all.push(iv);
            lock(win).iv_ms.push(iv);
            self.recv_iv_all.push(f32_from_u64(now - self.last_recv) / 1000.0);
        }
        // Same PC, same QPC clock: recv - capture is the true capture->received latency.
        // Two's-complement difference, same as the old `now as i64 - t_us as i64`.
        let lat = f32_from_i64(now.wrapping_sub(h.t_us).cast_signed()) / 1000.0;
        self.lat_all.push(lat);
        self.last_t = h.t_us;
        self.last_recv = now;
        self.frames += 1;
        self.bytes += len_u64(p.len());
        if key {
            self.kf += 1;
        }
        {
            let mut g = lock(win);
            g.frames += 1;
            g.bytes += len_u64(p.len()) + len_u64(HDR_LEN);
            g.lat_ms.push(lat);
            g.last_t = h.t_us;
            g.last_recv = now;
        }
        if let Some(o) = sinks.out.as_mut() {
            o.write_all(p)?;
        }
        if let Some(t) = sinks.ts.as_mut() {
            writeln!(t, "{},{},{},{}", h.t_us, now, p.len(), u8::from(key))?;
        }
        Ok(())
    }

    fn print_summary(&mut self, secs: f32, res: &anyhow::Result<()>) {
        let iv = pct(&mut self.iv_all);
        let rx = pct(&mut self.recv_iv_all);
        let lat = pct(&mut self.lat_all);
        let over = self.iv_all.iter().filter(|&&x| x > 25.0).count();
        eprintln!(
            "[fake_tv] summary: hello={} frames={} keyframes={} secs={:.1} fps={:.2} mbps={:.2} protocol_errors={}\n\
             [fake_tv]   capture interval ms p50={:.2} p99={:.2} max={:.2} (>25ms: {})\n\
             [fake_tv]   receive interval ms p50={:.2} p99={:.2} max={:.2}\n\
             [fake_tv]   capture->received ms p50={:.2} p99={:.2} max={:.2}  end={:?}",
            self.hello,
            self.frames,
            self.kf,
            secs,
            f32_from_u64(self.frames) / secs,
            f32_from_u64(self.bytes) * 8.0 / 1e6 / secs,
            self.errors,
            iv.0,
            iv.1,
            iv.2,
            over,
            rx.0,
            rx.1,
            rx.2,
            lat.0,
            lat.1,
            lat.2,
            res.as_ref().err().map(std::string::ToString::to_string)
        );
    }
}

fn session(
    sock: TcpStream,
    dump: Option<&str>,
    idr_every: Option<f64>,
    caps: Caps,
    audio: &Arc<Audio>,
    opts: AudioOpts,
) -> anyhow::Result<()> {
    let peer = sock.peer_addr()?.ip();
    let w = Arc::new(Mutex::new(sock.try_clone()?));
    // CAPS right after connect, before the first STATS (a real TV sends it once its decoder list is built).
    if let Some(c) = caps_json(caps, opts) {
        eprintln!("[fake_tv] sending CAPS {c}");
        write_msg(&mut *lock(&w), T_CAPS, 0, now_us(), &[c.to_string().as_bytes()])?;
    }
    let mut rd = sock;
    let done = Arc::new(AtomicBool::new(false));
    let win = Arc::new(Mutex::new(Win::default()));
    let mut sinks = Sinks {
        out: match dump {
            Some(p) => Some(BufWriter::new(File::create(p)?)),
            None => None,
        },
        ts: match dump {
            Some(p) => Some(BufWriter::new(File::create(format!("{p}.ts.csv"))?)),
            None => None,
        },
    };
    if let Some(t) = sinks.ts.as_mut() {
        writeln!(t, "t_us,recv_us,size,key")?;
    }

    // STATS + periodic IDR_REQUEST thread
    let st = {
        let (w, done, win, audio) = (Arc::clone(&w), Arc::clone(&done), Arc::clone(&win), Arc::clone(audio));
        std::thread::spawn(move || stats_loop(&w, &done, &win, idr_every, &audio))
    };
    let out = {
        let (done, audio) = (Arc::clone(&done), Arc::clone(audio));
        std::thread::spawn(move || audio::output_thread(&audio, &done))
    };
    let mut tone: Option<std::thread::JoinHandle<()>> = None;

    let t_start = Instant::now();
    let mut rx = Rx::new();
    let res: anyhow::Result<()> = loop {
        let (h, p) = match read_msg(&mut rd) {
            Ok(x) => x,
            Err(e) => break Err(e.into()),
        };
        if rx.on_msg(&h, &p, &w, &win, &mut sinks, (audio, peer))? == Flow::Bye {
            break Ok(());
        }
        if let (T_AUDIO_START, Some(amp), None) = (h.typ, opts.tone, tone.as_ref()) {
            let done = Arc::clone(&done);
            tone = Some(std::thread::spawn(move || audio::tone_thread(&done, amp)));
        }
    };
    done.store(true, SeqCst);
    let _ = st.join();
    let _ = out.join();
    if let Some(t) = tone {
        let _ = t.join();
    }
    audio.stop();
    if let Some(mut o) = sinks.out {
        o.flush()?;
    }
    if let Some(mut t) = sinks.ts {
        t.flush()?;
    }
    rx.print_summary(t_start.elapsed().as_secs_f32(), &res);
    Ok(())
}
