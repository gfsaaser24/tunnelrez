//! TCP listener and per-session threads.
//!
//! Per session:
//! - network thread (`lrt-session`): reads messages only. PING is answered here; HELLO / CONFIG / FRAME go
//!   into a bounded [`FrameQueue`]. It never waits for the decoder, so its arrival times are clean network
//!   numbers.
//! - decoder thread (`lrt-decode`): pops the queue and feeds AMediaCodec, waiting as long as needed for an
//!   input buffer. It creates the decoder as soon as HELLO arrives, so the decoder is ready for the first
//!   keyframe.
//! - stats thread (`lrt-stats`): CAPS once, 1 Hz STATS to the sender and logcat, and the no-output watchdog.

use crate::av1;
use crate::codec::{CodecKind, Decoder, Mode, StreamSpec, BUFFER_FLAG_CODEC_CONFIG};
use crate::frame_queue::{Frame, FrameQueue, Item, Push, DEFAULT_CAP, MAX_CAP, START_CAP};
use crate::metrics::{count_f32, max_sorted, percentiles, round1, us_to_ms};
use crate::pipeline::{lock, InFlight, Shared, Writer};
use crate::proto::{
    encode, parse_header, HEADER_LEN, PORT, T_AUDIO_START, T_AUDIO_STOP, T_BYE, T_CAPS, T_CONFIG, T_FRAME, T_HELLO,
    T_PING, T_PONG, T_STATS,
};
use crate::session_end::end_session;
use crate::util::{get_prop_i32, now_us, set_thread_priority};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Socket receive buffer (PROTOCOL.md).
const RCVBUF: i32 = 2 * 1024 * 1024;
/// How long one `dequeueInputBuffer` call may block. The decoder thread loops on it, checking for
/// session end / recover between calls, so this only bounds how fast it notices those.
const INPUT_POLL_US: i64 = 10_000;
/// Give up on one frame after waiting this long for an input buffer: the decoder is stuck.
const INPUT_WAIT_MAX_US: u64 = 1_000_000;
/// Arrival or render intervals above this count as a stutter in the logcat summary (1.5 frames at 60 fps).
const STUTTER_MS: f32 = 25.0;
/// A decoder that stalls this soon after it was created is taken to be broken with its key set: the next
/// one gets fewer optional keys. A stall later on (after minutes of good video) only recreates it.
const YOUNG_DECODER: Duration = Duration::from_secs(10);
/// A decoder created on HELLO that got no frame for this long is released, so the idle screen can show.
const UNUSED_DECODER: Duration = Duration::from_secs(3);
/// Frames whose decoder input is logged after each decoder start.
const IN_LOG_FRAMES: u32 = 8;

fn set_sock_buf(fd: i32, opt: i32, bytes: i32) {
    let len = libc::socklen_t::try_from(std::mem::size_of::<i32>()).expect("4 fits in socklen_t");
    // SAFETY: `fd` is an open socket owned by the caller; `bytes` outlives the call and `len` is its size.
    unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, opt, std::ptr::from_ref(&bytes).cast(), len) };
}

/// Accept loop. One client at a time; a new connection replaces the old one.
pub(crate) fn listener_thread(sh: &'static Shared) {
    let listener = loop {
        match TcpListener::bind(("0.0.0.0", PORT)) {
            Ok(l) => break l,
            Err(e) => {
                log::error!("bind :{PORT} failed: {e} (retrying)");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };
    set_sock_buf(listener.as_raw_fd(), libc::SO_RCVBUF, RCVBUF);
    log::info!("listening on 0.0.0.0:{PORT}");
    let mut current: Option<(TcpStream, JoinHandle<()>)> = None;
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        if let Some((old, h)) = current.take() {
            log::info!("new client {peer} replaces the old one");
            let _ = old.shutdown(Shutdown::Both);
            let _ = h.join();
        }
        let _ = stream.set_nodelay(true);
        set_sock_buf(stream.as_raw_fd(), libc::SO_RCVBUF, RCVBUF);
        let Ok(ctl) = stream.try_clone() else {
            continue;
        };
        let h = std::thread::Builder::new()
            .name("lrt-session".into())
            .spawn(move || session(sh, stream, &peer))
            .expect("spawn session thread");
        current = Some((ctl, h));
    }
}

fn parse_hello(p: &[u8]) -> Result<StreamSpec, String> {
    let v: serde_json::Value = serde_json::from_slice(p).map_err(|e| e.to_string())?;
    let codec = v.get("codec").and_then(serde_json::Value::as_str).unwrap_or("h264");
    let num =
        |k: &str, d: i32| v.get(k).and_then(serde_json::Value::as_i64).and_then(|x| i32::try_from(x).ok()).unwrap_or(d);
    Ok(StreamSpec {
        kind: CodecKind::parse(codec).ok_or_else(|| format!("unknown codec {codec}"))?,
        width: num("width", 1920),
        height: num("height", 1080),
        fps: num("fps", 60),
    })
}

fn send(w: &Writer, typ: u8, payload: &[u8]) -> bool {
    let msg = encode(typ, 0, now_us(), payload);
    lock(w).write_all(&msg).is_ok()
}

/// Network thread for one client.
fn session(sh: &'static Shared, mut stream: TcpStream, peer: &str) {
    set_thread_priority(-16);
    log::info!("client connected: {peer}");
    let connected_at = Instant::now();
    sh.reset_stats();
    sh.connected.store(true, Ordering::SeqCst);
    sh.idle_dirty.store(true, Ordering::SeqCst);
    let w: Writer = Arc::new(Mutex::new(stream.try_clone().expect("clone socket")));
    *lock(&sh.writer) = Some(Arc::clone(&w));

    let cap = usize::try_from(get_prop_i32("debug.lrt.rxq", 0)).ok().filter(|&c| c > 0).unwrap_or(DEFAULT_CAP);
    let start_cap =
        usize::try_from(get_prop_i32("debug.lrt.rxq_start", 0)).ok().filter(|&c| c > 0).unwrap_or(START_CAP);
    let queue = Arc::new(FrameQueue::new(cap.min(MAX_CAP), start_cap));
    log::info!("receive queue: {} frames ({} while a decoder starts)", queue.cap(), queue.start_cap());
    let stop = Arc::new(AtomicBool::new(false));
    let stats_h = {
        let (w, stop, q) = (Arc::clone(&w), Arc::clone(&stop), Arc::clone(&queue));
        std::thread::Builder::new()
            .name("lrt-stats".into())
            .spawn(move || stats_thread(sh, &w, &stop, &q))
            .expect("spawn stats thread")
    };
    let dec_h = {
        let q = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("lrt-decode".into())
            .spawn(move || DecodeLoop::new(sh, q, connected_at).run())
            .expect("spawn decoder thread")
    };

    let reason = read_loop(sh, &mut stream, &w, &queue);
    log::info!("client {peer} disconnected: {reason}");
    sh.audio.stop();
    // Shuts the socket down before joining: a worker may be blocked writing to it.
    end_session(&queue, &stop, &stream, vec![dec_h, stats_h]);
    *lock(&sh.writer) = None;
    sh.destroy_decoder();
    sh.connected.store(false, Ordering::SeqCst);
    sh.idle_dirty.store(true, Ordering::SeqCst);
}

/// Read messages until the connection ends. Returns why it ended.
fn read_loop(sh: &'static Shared, stream: &mut TcpStream, w: &Writer, queue: &FrameQueue) -> String {
    let peer_ip = stream.peer_addr().map(|a| a.ip()).ok();
    let mut hdr = [0u8; HEADER_LEN];
    loop {
        if let Err(e) = stream.read_exact(&mut hdr) {
            return format!("read: {e}");
        }
        let h = match parse_header(&hdr) {
            Ok(h) => h,
            Err(e) => return e.to_string(),
        };
        let mut payload = vec![0u8; h.payload_len()];
        if let Err(e) = stream.read_exact(&mut payload) {
            return format!("read payload: {e}");
        }
        let recv_us = now_us();
        {
            let mut st = lock(&sh.stats);
            st.bytes_in += u64::from(h.len) + 20;
            if h.typ == T_FRAME {
                st.frames_in += 1;
                if st.last_arrival_us != 0 {
                    let iv = us_to_ms(recv_us.saturating_sub(st.last_arrival_us));
                    st.arrival_ms.push(iv);
                }
                st.last_arrival_us = recv_us;
            }
        }
        match h.typ {
            T_HELLO => {
                queue.push(Item::Hello(payload));
            }
            T_CONFIG => {
                queue.push(Item::Config(payload));
            }
            T_FRAME => {
                let key = h.is_key();
                if key {
                    sh.keyframe_arrived();
                }
                let res = queue.push(Item::Frame(Frame { key, sender_t_us: h.t_us, recv_us, data: payload }));
                on_push(sh, res);
            }
            T_PING => {
                let echo = payload.first_chunk::<8>().map_or(h.t_us, |b| u64::from_le_bytes(*b));
                let mut p = [0u8; 16];
                p[..8].copy_from_slice(&echo.to_le_bytes());
                p[8..].copy_from_slice(&now_us().to_le_bytes());
                if !send(w, T_PONG, &p) {
                    return "write PONG failed".into();
                }
            }
            T_AUDIO_START => {
                if let Some(ip) = peer_ip {
                    sh.audio.start(sh, ip, &payload);
                }
            }
            T_AUDIO_STOP => sh.audio.stop(),
            T_BYE => return "BYE".into(),
            _ => {} // unknown: payload already consumed
        }
    }
}

/// Network-thread reaction to a queue push.
fn on_push(sh: &Shared, res: Push) {
    match res {
        Push::Queued | Push::Closed => {}
        Push::Flushed { dropped } => {
            lock(&sh.stats).dropped += u64::try_from(dropped).expect("usize fits in u64");
            log::warn!("receive queue full at a keyframe: dropped {dropped} older frames");
        }
        Push::Overflow { dropped } => {
            lock(&sh.stats).dropped += u64::try_from(dropped).expect("usize fits in u64");
            log::warn!("receive queue full: dropped {dropped} frames, waiting for a keyframe");
            sh.request_idr("receive queue overflow");
        }
        Push::WaitingForKey => {
            lock(&sh.stats).dropped += 1;
            // Only sends if the earlier request is old and still unanswered.
            sh.request_idr("receive queue waiting for keyframe");
        }
    }
}

/// How AV1 temporal units go into the decoder (`debug.lrt.av1.*`, read per session).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Av1Feed {
    /// Strip temporal delimiters / padding (MP4 sample form). `debug.lrt.av1.raw=1` feeds the TU as sent.
    sample_form: bool,
    /// Queue the `av1C` record as a codec-config buffer first. `debug.lrt.av1.csd=0` turns it off.
    csd: bool,
}

impl Av1Feed {
    fn load() -> Av1Feed {
        Av1Feed {
            sample_form: get_prop_i32("debug.lrt.av1.raw", 0) == 0,
            csd: get_prop_i32("debug.lrt.av1.csd", 1) != 0,
        }
    }
}

/// The current decoder's life, as the decoder thread tracks it.
#[derive(Debug)]
struct DecoderLife {
    born: Instant,
    /// `Shared::frames_shown` when it was created.
    shown_at_birth: u64,
    /// It showed a picture (the queue's startup phase has ended).
    showed: bool,
    /// Frames queued into it.
    fed: u32,
    /// Config bytes it got as a codec-config buffer.
    fed_config: Option<Vec<u8>>,
}

/// Decoder-thread state for one session.
struct DecodeLoop {
    sh: &'static Shared,
    queue: Arc<FrameQueue>,
    connected_at: Instant,
    av1_feed: Av1Feed,
    hello: Option<StreamSpec>,
    config: Option<Vec<u8>>,
    /// Codec-config bytes for the decoder: the H.264 / HEVC parameter sets as sent, or the AV1 `av1C` record.
    csd: Option<Vec<u8>>,
    /// AV1: the sequence header OBU from CONFIG (put in front of a keyframe that has none).
    av1_seq: Option<Vec<u8>>,
    life: Option<DecoderLife>,
    need_key: bool,
    last_pts: u64,
    /// Frames are being dropped because the app is in the background (logged once per episode).
    background: bool,
}

impl DecodeLoop {
    fn new(sh: &'static Shared, queue: Arc<FrameQueue>, connected_at: Instant) -> DecodeLoop {
        // A new session has no decoder yet: requests left over from before it (the app coming to the front
        // with no client, an old session's error) must not tear down the decoder this session creates on HELLO.
        for flag in [&sh.recover, &sh.stall, &sh.dec_error] {
            flag.store(false, Ordering::SeqCst);
        }
        let av1_feed = Av1Feed::load();
        if av1_feed != (Av1Feed { sample_form: true, csd: true }) {
            log::info!("AV1 feed override: {av1_feed:?}");
        }
        DecodeLoop {
            sh,
            queue,
            connected_at,
            av1_feed,
            hello: None,
            config: None,
            csd: None,
            av1_seq: None,
            life: None,
            need_key: true,
            last_pts: 0,
            background: false,
        }
    }

    fn run(mut self) {
        set_thread_priority(-16);
        loop {
            match self.queue.pop(Duration::from_millis(100)) {
                Some(Item::Hello(p)) => self.on_hello(&p),
                Some(Item::Config(p)) => self.on_config(p),
                Some(Item::Frame(f)) => self.on_frame(&f),
                None if self.queue.is_closed() => return,
                None => self.release_unused_decoder(),
            }
            self.check_first_picture();
        }
    }

    fn drop_frame(&self) {
        lock(&self.sh.stats).dropped += 1;
    }

    fn reset_decoder(&mut self) {
        self.sh.destroy_decoder();
        self.life = None;
        self.need_key = true;
    }

    /// The current decoder showed its first picture: end the queue's startup phase.
    fn check_first_picture(&mut self) {
        let Some(life) = self.life.as_mut() else { return };
        if life.showed || self.sh.frames_shown() <= life.shown_at_birth {
            return;
        }
        life.showed = true;
        self.queue.end_startup();
        log::info!(
            "first picture: {} ms after decoder start, {} ms after connect",
            life.born.elapsed().as_millis(),
            self.connected_at.elapsed().as_millis()
        );
    }

    /// A decoder made on HELLO that never got a frame: release it so the idle screen can show.
    fn release_unused_decoder(&mut self) {
        if self.life.as_ref().is_some_and(|l| l.fed == 0 && l.born.elapsed() > UNUSED_DECODER) {
            log::info!("no video {} s after HELLO: releasing the decoder", UNUSED_DECODER.as_secs());
            self.reset_decoder();
        }
    }

    fn on_hello(&mut self, p: &[u8]) {
        match parse_hello(p) {
            Ok(h) => {
                log::info!("HELLO {}", String::from_utf8_lossy(p));
                let changed = self.hello.is_none_or(|o| o.kind != h.kind || o.width != h.width || o.height != h.height);
                if changed {
                    self.reset_decoder();
                    self.config = None;
                    self.csd = None;
                    self.av1_seq = None;
                }
                self.hello = Some(h);
                // HELLO has codec, size and rate: enough to configure. Creating the decoder now (~80 ms)
                // means it is ready when the first keyframe comes, instead of frames piling up meanwhile.
                if !self.sh.has_decoder() && self.sh.visible() && self.create_decoder() {
                    log::info!("decoder created on HELLO");
                }
            }
            Err(e) => log::error!("bad HELLO: {e} ({})", String::from_utf8_lossy(p)),
        }
    }

    fn on_config(&mut self, p: Vec<u8>) {
        if self.config.as_deref() == Some(&p[..]) {
            return;
        }
        log::info!("CONFIG {} bytes", p.len());
        // A different config while decoding (resolution / profile change) -> new decoder at the next keyframe.
        if self.life.as_ref().is_some_and(|l| l.fed_config.as_ref().is_some_and(|c| *c != p)) {
            log::info!("CONFIG changed, recreating decoder");
            self.reset_decoder();
        }
        let av1 = self.hello.is_some_and(|h| h.kind == CodecKind::Av1);
        if av1 {
            self.av1_seq = av1::seq_header_obu(&p).map(<[u8]>::to_vec);
            self.csd = if let Some((rec, sh)) = av1::av1c_record(&p) {
                let c = sh.color;
                log::info!(
                    "AV1 sequence header: profile {} level_idx {} tier {} {}x{} {}-bit mono={} ss={}{} csp={} \
                     -> av1C {} bytes",
                    sh.profile,
                    sh.level_idx,
                    sh.tier,
                    sh.max_width,
                    sh.max_height,
                    c.bit_depth,
                    c.mono_chrome,
                    u8::from(c.subsampling_x),
                    u8::from(c.subsampling_y),
                    c.chroma_sample_position,
                    rec.len()
                );
                Some(rec)
            } else {
                log::warn!("AV1 CONFIG has no sequence header I can parse: decoding with the in-band one only");
                None
            };
        } else {
            self.csd = Some(p.clone());
        }
        self.config = Some(p);
    }

    /// Create the decoder (no codec config yet: that goes in before the first keyframe). False if there is
    /// no window or the decoder could not be created.
    fn create_decoder(&mut self) -> bool {
        let Some(spec) = self.hello else {
            log::warn!("FRAME before HELLO; assuming h264 1920x1080@60");
            self.hello = Some(StreamSpec { kind: CodecKind::H264, width: 1920, height: 1080, fps: 60 });
            return false;
        };
        let rung = self.sh.rung(spec.kind);
        let mut g = self.sh.dec_write();
        let win = lock(&self.sh.window);
        let Some(win) = win.as_ref() else {
            return false;
        };
        let t0 = Instant::now();
        let d = match Decoder::create(spec, win, rung) {
            Ok(d) => d,
            Err(e) => {
                log::error!("decoder create failed: {e}");
                return false;
            }
        };
        log::info!("decoder ready in {} ms (key set {}/{})", t0.elapsed().as_millis(), d.rung + 1, d.rungs);
        lock(&self.sh.inflight).clear();
        let render = u32::try_from(get_prop_i32("debug.lrt.render", 1)).unwrap_or(1);
        self.sh.render_mode.store(render, Ordering::Relaxed);
        *g = Some(d);
        drop(g);
        self.sh.decoder_started();
        self.queue.begin_startup();
        self.life = Some(DecoderLife {
            born: Instant::now(),
            shown_at_birth: self.sh.frames_shown(),
            showed: false,
            fed: 0,
            fed_config: None,
        });
        true
    }

    /// Before a keyframe: a decoder exists and has the current codec config. False = drop the frame.
    fn ensure_decoder(&mut self) -> bool {
        if !self.sh.has_decoder() && !self.create_decoder() {
            return false;
        }
        let Some(spec) = self.hello else { return false };
        let fed = self.life.as_ref().and_then(|l| l.fed_config.as_ref());
        if self.config.is_some() && fed == self.config.as_ref() {
            return true;
        }
        let csd = match (spec.kind, self.csd.as_ref()) {
            (CodecKind::Av1, Some(_)) if !self.av1_feed.csd => None,
            (_, Some(c)) => Some(c.clone()),
            (CodecKind::Av1, None) => None,
            (_, None) => {
                self.sh.request_idr("keyframe without CONFIG");
                return false;
            }
        };
        if let Some(csd) = csd {
            let g = self.sh.dec_read();
            let Some(dec) = g.as_ref() else { return false };
            match dec.queue(&[&csd], 0, BUFFER_FLAG_CODEC_CONFIG, 100_000, |_| {}) {
                Ok(Some(_)) => log::info!("codec config queued: {} bytes", csd.len()),
                other => {
                    log::error!("queue codec config failed: {other:?}");
                    return false;
                }
            }
        }
        if let Some(life) = self.life.as_mut() {
            life.fed_config.clone_from(&self.config);
        }
        true
    }

    fn on_frame(&mut self, f: &Frame) {
        if self.sh.recover.swap(false, Ordering::SeqCst) {
            log::info!("recover: new decoder at the next keyframe");
            self.reset_decoder();
        }
        if self.sh.dec_error.swap(false, Ordering::SeqCst) {
            log::warn!("decoder error -> recreate");
            self.reset_decoder();
        }
        if self.sh.stall.swap(false, Ordering::SeqCst) {
            self.on_stall("frames arrive but no picture was shown for 2 s");
        }
        if !self.sh.visible() {
            // Background: keep the connection (PING/PONG/STATS go on), drop frames quietly. The main thread
            // sets `recover` and requests a keyframe when the app comes back.
            if self.sh.has_decoder() {
                self.reset_decoder();
            }
            if !self.background {
                self.background = true;
                log::info!("in background: dropping frames until the app is visible again");
            }
            self.need_key = true;
            self.drop_frame();
            return;
        }
        if self.background {
            self.background = false;
            log::info!("visible again: resuming decode");
        }
        if !self.sh.has_decoder() {
            self.need_key = true;
        }
        if self.need_key && !f.key {
            self.drop_frame();
            self.sh.request_idr("waiting for keyframe");
            return;
        }
        if f.key && !self.ensure_decoder() {
            self.drop_frame();
            return;
        }
        // Local receive time as pts (unique, monotonic): the output side uses it for latency stats, and it
        // keeps SurfaceFlinger from treating sender-clock timestamps as a desired present time.
        let pts = f.recv_us.max(self.last_pts + 1);
        self.last_pts = pts;
        match self.feed(f, pts) {
            Fed::Queued => self.need_key = false,
            Fed::Dropped => self.drop_frame(),
            Fed::Stalled => {
                self.drop_frame();
                self.on_stall(&format!("no decoder input buffer for {} ms", INPUT_WAIT_MAX_US / 1000));
            }
            Fed::Failed(e) => {
                log::error!("queue failed: {e}");
                self.drop_frame();
                self.reset_decoder();
                self.sh.request_idr("decoder error");
            }
        }
    }

    /// The decoder stopped working. Replace it at the next keyframe; if it was young, the next one gets
    /// fewer optional format keys (the MTK decoders have keys that work for one codec and hang another).
    fn on_stall(&mut self, why: &str) {
        let young = self.life.as_ref().is_some_and(|l| l.born.elapsed() < YOUNG_DECODER);
        let info = self.sh.dec_read().as_ref().map(|d| (d.kind, d.name.clone(), d.keys.clone(), d.rung, d.rungs));
        if let Some((kind, name, keys, rung, rungs)) = info {
            let age = self.life.as_ref().map_or(0, |l| l.born.elapsed().as_millis());
            // Only a young decoder lowers the key set; a late stall (after good video) keeps it.
            let next = if young { self.sh.escalate(kind, rung, rungs) } else { None };
            match next {
                Some(next) => log::warn!(
                    "decoder stalled ({why}) {age} ms after start: {name} with keys [{keys}]; next decoder uses \
                     key set {}/{rungs}",
                    next + 1
                ),
                None => {
                    log::warn!("decoder stalled ({why}) {age} ms after start: {name} with keys [{keys}]; recreating");
                }
            }
        }
        self.reset_decoder();
        self.sh.force_idr("decoder stalled");
    }

    /// Queue one frame into the decoder, waiting for an input buffer in short steps.
    fn feed(&mut self, f: &Frame, pts: u64) -> Fed {
        let start = now_us();
        let Some(kind) = self.sh.dec_read().as_ref().map(|d| d.kind) else {
            return Fed::Dropped;
        };
        let (parts, av1_info) = if kind == CodecKind::Av1 && self.av1_feed.sample_form {
            let seq = if f.key { self.av1_seq.as_deref() } else { None };
            let (parts, info) = av1::sample_parts(&f.data, seq);
            (parts, Some(info))
        } else {
            (vec![&f.data[..]], None)
        };
        loop {
            if self.queue.is_closed()
                || self.sh.recover.load(Ordering::SeqCst)
                || self.sh.dec_error.load(Ordering::SeqCst)
            {
                return Fed::Dropped; // handled at the next frame (or session end)
            }
            let g = self.sh.dec_read();
            let Some(dec) = g.as_ref() else {
                return Fed::Dropped;
            };
            let tunneled = dec.mode == Mode::Tunneled;
            // The in-flight entry goes in before queueInputBuffer: the output thread may get the picture before
            // this thread runs again, and must find its entry to match it.
            let inflight = &self.sh.inflight;
            let publish = |queued_us| {
                if !tunneled {
                    lock(inflight).push_back(InFlight { pts, queued_us, sender_t_us: f.sender_t_us });
                }
            };
            let res = dec.queue(&parts, pts, 0, INPUT_POLL_US, publish);
            drop(g);
            match res {
                Ok(Some(queued_us)) => {
                    let wait = us_to_ms(queued_us.saturating_sub(start));
                    let mut st = lock(&self.sh.stats);
                    if tunneled {
                        // No output buffers in tunnel mode: count frames accepted by the decoder as output.
                        st.frames_out += 1;
                        st.decode_ms.push(us_to_ms(queued_us.saturating_sub(pts)));
                        // receive -> accepted
                    }
                    st.in_wait_ms.push(wait);
                    drop(st);
                    if let Some(life) = self.life.as_mut() {
                        life.fed += 1;
                        if life.fed <= IN_LOG_FRAMES {
                            let bytes: usize = parts.iter().map(|p| p.len()).sum();
                            log::info!(
                                "input #{}: key={} {} bytes (sent {}) wait {wait:.1} ms{}",
                                life.fed,
                                f.key,
                                bytes,
                                f.data.len(),
                                av1_info.map_or(String::new(), |i| format!(
                                    ", av1: {} OBUs stripped, seq header added={}",
                                    i.stripped, i.seq_added
                                ))
                            );
                        }
                    }
                    return Fed::Queued;
                }
                Ok(None) => {
                    if now_us().saturating_sub(start) > INPUT_WAIT_MAX_US {
                        return Fed::Stalled;
                    }
                }
                Err(e) => {
                    // A submit that failed after the entry was published: take it back out (pts is unique).
                    lock(&self.sh.inflight).retain(|x| x.pts != pts);
                    return Fed::Failed(e);
                }
            }
        }
    }
}

/// Result of [`DecodeLoop::feed`].
#[derive(Debug)]
enum Fed {
    Queued,
    /// Session ending, recovering, or decoder gone: drop this frame quietly.
    Dropped,
    /// No input buffer for [`INPUT_WAIT_MAX_US`].
    Stalled,
    Failed(String),
}

/// CAPS to the sender, once the decoder list is ready. True when sent (or nothing to send ever).
fn send_caps(sh: &Shared, w: &Writer) -> bool {
    let Some(caps) = sh.caps.get() else { return false };
    if send(w, T_CAPS, caps.as_bytes()) {
        log::info!("CAPS sent ({} bytes)", caps.len());
    }
    true
}

/// CAPS once, then 1 Hz STATS to the sender + the same line on logcat (tag LRT).
fn stats_thread(sh: &'static Shared, w: &Writer, stop: &AtomicBool, queue: &FrameQueue) {
    let mut caps_sent = send_caps(sh, w);
    let mut last = Instant::now();
    // Seconds in a row with frames arriving but none displayed (decoder present, app visible).
    let mut stalled_secs = 0u32;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        if !caps_sent {
            caps_sent = send_caps(sh, w);
        }
        let el = last.elapsed();
        if el < Duration::from_secs(1) {
            continue;
        }
        last = Instant::now();
        let secs = el.as_secs_f32();
        let mut s = {
            let mut st = lock(&sh.stats);
            let snap = Snapshot {
                frames_in: st.frames_in,
                frames_out: st.frames_out,
                bytes_in: st.bytes_in,
                dropped: st.dropped,
                late: st.late,
                out_unmatched: st.out_unmatched,
                decode_ms: std::mem::take(&mut st.decode_ms),
                lat_ms: std::mem::take(&mut st.lat_ms),
                interval_ms: std::mem::take(&mut st.interval_ms),
                arrival_ms: std::mem::take(&mut st.arrival_ms),
                in_wait_ms: std::mem::take(&mut st.in_wait_ms),
                disp_t_us: st.disp_t_us,
                disp_us: st.disp_us,
            };
            st.frames_in = 0;
            st.frames_out = 0;
            st.bytes_in = 0;
            snap
        };
        let qw = queue.take_window();
        let idr_sent = sh.take_idr_sent();
        let (d50, d99) = percentiles(&mut s.decode_ms);
        let (l50, l99) = percentiles(&mut s.lat_ms);
        let (i50, i99) = percentiles(&mut s.interval_ms);
        let (a50, a99) = percentiles(&mut s.arrival_ms);
        let (w50, w99) = percentiles(&mut s.in_wait_ms);
        let stutter = s.interval_ms.iter().filter(|&&x| x > STUTTER_MS).count();
        let arr_stutter = s.arrival_ms.iter().filter(|&&x| x > STUTTER_MS).count();
        let queue_inflight = lock(&sh.inflight).len();
        let (codec, mode, keys) = match sh.dec_read().as_ref() {
            Some(d) => (d.name.clone(), Some(d.mode), d.keys.clone()),
            None => (String::new(), None, String::new()),
        };
        // Watchdog: the decoder may hang, or be reclaimed by another app without an error.
        if sh.visible() && mode == Some(Mode::Buffers) && s.frames_in > 0 && s.frames_out == 0 {
            stalled_secs += 1;
            if stalled_secs >= 2 {
                stalled_secs = 0;
                log::warn!("watchdog: frames arrive but none are displayed for 2 s -> new decoder");
                sh.stall.store(true, Ordering::SeqCst);
            }
        } else {
            stalled_secs = 0;
        }
        let rate = |n: u32| round1(count_f32(u64::from(n)) / secs);
        let mut js = serde_json::json!({
            "fps_in": rate(s.frames_in),
            "fps_out": rate(s.frames_out),
            "dropped": s.dropped,
            "decode_ms_p50": round1(d50),
            "decode_ms_p99": round1(d99),
            "queue": queue_inflight,
            // bits per millisecond = kbit/s
            "recv_kbps": s.bytes_in * 8 / u64::try_from(el.as_millis()).unwrap_or(1).max(1),
            "late": s.late,
            "out_unmatched": s.out_unmatched,
            "codec": codec,
            "dec_keys": keys,
            "rxq_depth": qw.depth,
            "rxq_max": qw.max_depth,
            "rxq_dropped": qw.dropped,
            "rxq_cap": qw.limit,
            "in_wait_ms_p50": round1(w50),
            "in_wait_ms_p99": round1(w99),
            "idr_sent_s": rate(idr_sent),
            "arrival_ms_p50": round1(a50),
            "arrival_ms_p99": round1(a99),
            "arrival_ms_max": round1(max_sorted(&s.arrival_ms)),
            "arrival_gaps": arr_stutter,
        });
        // Display time of the newest released frame (not available in tunnel mode: no output buffers).
        if s.disp_us != 0 && mode != Some(Mode::Tunneled) {
            js["disp_t_us"] = s.disp_t_us.into();
            js["disp_us"] = s.disp_us.into();
        }
        sh.audio.stats(&mut js, secs);
        let js = js.to_string();
        log::info!("{js}");
        log::info!(
            "extra lat_ms_p50={l50:.1} lat_ms_p99={l99:.1} interval_ms_p50={i50:.1} p99={i99:.1} max={:.1} stutter={stutter}",
            max_sorted(&s.interval_ms),
        );
        if !send(w, T_STATS, js.as_bytes()) {
            break;
        }
    }
}

/// One stats window, taken under the stats lock.
struct Snapshot {
    frames_in: u32,
    frames_out: u32,
    bytes_in: u64,
    dropped: u64,
    late: u64,
    out_unmatched: u64,
    decode_ms: Vec<f32>,
    lat_ms: Vec<f32>,
    interval_ms: Vec<f32>,
    arrival_ms: Vec<f32>,
    in_wait_ms: Vec<f32>,
    disp_t_us: u64,
    disp_us: u64,
}
