//! Audio on the TV (PROTOCOL.md, "Audio"): a UDP thread hands AUDIO datagrams from the current sender to the
//! `AAudio` data callback through a wait-free single-producer ring (`rtrb`); the callback feeds them to the [`Playout`] and
//! pulls each burst from it, for the time `AAudio` will present it (`AAudioStream_getTimestamp`) and as late as
//! the video is shown. The callback never waits on another thread and the playout does not allocate.

use crate::jitter::{parse_payload, parse_start, Packet, Playout, Recycle, Snapshot, VideoTrack, RATE};
use crate::pipeline::{lock, Shared};
use crate::proto::{parse_header, HEADER_LEN, PORT, T_AUDIO};
use crate::util::{get_prop_i32, now_ns, now_us, set_thread_priority};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode, AudioStream,
    AudioStreamBuilder, AudioStreamDataCallback, Clockid,
};
use std::net::{IpAddr, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// UDP receive buffer: ~1 s of audio, so a burst after a Wi-Fi gap is not lost in the kernel.
const UDP_RCVBUF: i32 = 256 * 1024;
/// Datagrams waiting for the audio callback (1.3 s), and datagrams waiting to be freed by the UDP thread.
const INGEST_CAP: usize = 256;
const RECYCLE_CAP: usize = 512;
/// `AAudio` buffer in bursts at the start: one, the least `AAudio` allows. The tuner below adds bursts after
/// underruns, so a device that needs more gets it within seconds.
const BUFFER_BURSTS: i32 = 1;
/// The buffer grows by one burst after a second with `AAudio` underruns (like Oboe's latency tuner), up to this.
const BUFFER_BURSTS_MAX: i32 = 8;
const TUNE_EVERY: Duration = Duration::from_secs(1);
/// How often the `AAudio` timestamp and the video delay are sampled.
const TIMESTAMP_EVERY: Duration = Duration::from_millis(50);
const VIDEO_EVERY: Duration = Duration::from_millis(100);
/// Video delay samples (one per 100 ms): 2 s of warm-up left out, then the median of 2 s. How old the newest
/// shown frame may be to count.
const VIDEO_WINDOW: usize = 20;
const VIDEO_WARMUP: usize = 20;
const VIDEO_STALE_US: u64 = 2_000_000;

/// The audio stream of the current session.
#[derive(Debug)]
struct Active {
    peer: IpAddr,
    sid: u32,
    /// Datagrams (with their arrival time) to the audio callback.
    ingest: rtrb::Producer<(Packet, u64)>,
    /// Datagrams the playout is done with, freed by the UDP thread (never on the audio callback).
    recycle: rtrb::Consumer<Packet>,
    /// The playout's latest numbers, published by the callback.
    snapshot: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// What the audio callback owns across stream reopens: the playout and the datagrams coming in.
#[derive(Debug)]
struct Feed {
    playout: Playout,
    ingest: rtrb::Consumer<(Packet, u64)>,
    snapshot: Arc<Mutex<Snapshot>>,
}

/// Output-side numbers for STATS.
#[derive(Debug, Default, Clone)]
struct OutInfo {
    out_ms: f32,
    mode: &'static str,
    xruns: i32,
}

/// Audio state shared by the network, UDP, output and stats threads.
#[derive(Debug, Default)]
pub(crate) struct Audio {
    active: Mutex<Option<Active>>,
    /// Datagrams accepted since the last STATS.
    pkts: AtomicU64,
    /// Datagrams dropped because the callback had not taken the earlier ones (session total).
    ingest_full: AtomicU64,
    /// DSCP of the last accepted datagram (+1; 0 = unknown): shows whether the sender's voice priority mark
    /// survives the network.
    dscp: AtomicU8,
    out: Mutex<OutInfo>,
}

impl Audio {
    /// `AUDIO_START` from the session's peer: (re)start audio.
    pub(crate) fn start(&self, sh: &'static Shared, peer: IpAddr, payload: &[u8]) {
        let sid = match parse_start(payload) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("AUDIO_START ignored: {e}");
                return;
            }
        };
        self.stop();
        // Both rings are wait-free for each side (`rtrb`): the callback never waits for the UDP thread.
        let (ingest, ingest_rx) = rtrb::RingBuffer::new(INGEST_CAP);
        let (mut recycle_tx, recycle) = rtrb::RingBuffer::new(RECYCLE_CAP);
        let to_recycle = Recycle(Box::new(move |p| {
            let _ = recycle_tx.push(p); // full: dropped on the callback (the UDP thread drains it per datagram)
        }));
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let feed =
            Feed { playout: Playout::with_recycle(to_recycle), ingest: ingest_rx, snapshot: Arc::clone(&snapshot) };
        let feed = Arc::new(Mutex::new(feed));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let s = Arc::clone(&stop);
            std::thread::Builder::new().name("lrt-audio".into()).spawn(move || output_thread(sh, &feed, &s))
        };
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("spawn audio thread: {e}");
                return;
            }
        };
        log::info!("audio on: sid {sid:08x} from {peer}");
        self.ingest_full.store(0, Ordering::Relaxed);
        *lock(&self.active) = Some(Active { peer, sid, ingest, recycle, snapshot, stop, thread });
    }

    /// `AUDIO_STOP`, BYE or the session ending: stop playing and free the output.
    pub(crate) fn stop(&self) {
        let Some(mut a) = lock(&self.active).take() else { return };
        a.stop.store(true, Ordering::SeqCst);
        if let Some(t) = a.thread.take() {
            let _ = t.join();
        }
        *lock(&self.out) = OutInfo::default();
        log::info!("audio off");
    }

    /// Hand a datagram from `from` with `sid` to the audio callback, if it belongs to the current stream; also
    /// free the datagrams the callback is done with. True when it was taken.
    fn deliver(&self, from: IpAddr, sid: u32, pkt: Packet, arrive_us: u64) -> bool {
        let mut g = lock(&self.active);
        let Some(a) = g.as_mut().filter(|a| a.peer == from && a.sid == sid) else { return false };
        while a.recycle.pop().is_ok() {}
        if a.ingest.push((pkt, arrive_us)).is_err() {
            self.ingest_full.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Add the audio keys to a STATS object (nothing when audio is off).
    pub(crate) fn stats(&self, js: &mut serde_json::Value, secs: f32) {
        let Some(snapshot) = lock(&self.active).as_ref().map(|a| Arc::clone(&a.snapshot)) else { return };
        let s = *lock(&snapshot);
        let out = lock(&self.out).clone();
        let pkts = self.pkts.swap(0, Ordering::SeqCst);
        let ms = |frames: u64| frames * 1000 / u64::from(RATE);
        let t = s.totals;
        js["a_buf_ms"] = round1(s.buf_ms).into();
        js["a_target_ms"] = round1(s.target_ms).into();
        js["a_delay_ms"] = round1(s.delay_ms).into();
        js["a_out_ms"] = round1(out.out_ms).into();
        js["a_sync"] = (if s.video_sync { "video" } else { "audio" }).into();
        js["a_mode"] = out.mode.into();
        js["a_pkts_s"] = round1(crate::metrics::count_f32(pkts) / secs).into();
        js["a_underruns"] = t.underruns.into();
        js["a_drops"] = (t.drops + self.ingest_full.load(Ordering::Relaxed)).into();
        js["a_lost"] = t.lost.into();
        js["a_skip_ms"] = ms(t.skip_frames).into();
        js["a_insert_ms"] = ms(t.insert_frames).into();
        js["a_slips"] = t.slips.into();
        js["a_adjust_ms"] = ms(t.adjust_frames).into();
        js["a_xruns"] = out.xruns.into();
        if let Some(d) = self.dscp.load(Ordering::Relaxed).checked_sub(1) {
            js["a_dscp"] = d.into();
        }
        if let Some((sender_t, tv_t)) = s.play {
            js["a_play_t_us"] = sender_t.into();
            js["a_play_us"] = tv_t.into();
        }
    }
}

fn round1(x: f32) -> f64 {
    crate::metrics::round1(x)
}

fn set_int_opt(sock: &UdpSocket, level: i32, name: i32, value: i32) {
    let len = libc::socklen_t::try_from(std::mem::size_of::<i32>()).expect("4 fits in socklen_t");
    // SAFETY: `sock` is an open socket; `value` outlives the call and `len` is its size.
    unsafe { libc::setsockopt(sock.as_raw_fd(), level, name, std::ptr::from_ref(&value).cast(), len) };
}

/// One datagram into `buf`: its length, the sender's IP and the IP TOS byte (with `IP_RECVTOS` on).
fn recv_with_tos(sock: &UdpSocket, buf: &mut [u8]) -> Option<(usize, IpAddr, Option<u8>)> {
    // SAFETY: all-zero is a valid `sockaddr_in` and `msghdr`.
    let mut from: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut control = [0u64; 8]; // room for one IP_TOS cmsg, u64-aligned
                                 // SAFETY: as above.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = std::ptr::from_mut(&mut from).cast();
    msg.msg_namelen = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_in>()).ok()?;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    // SAFETY: `msg` points at live locals (`from`, `iov` -> `buf`, `control`) whose sizes it states.
    let n = usize::try_from(unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, 0) }).ok()?;
    let ip = IpAddr::from(u32::from_be(from.sin_addr.s_addr).to_be_bytes());
    let mut tos = None;
    // SAFETY: `msg` was filled by recvmsg; the CMSG macros walk its control buffer within `msg_controllen`.
    let mut c = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    while !c.is_null() {
        // SAFETY: `c` is a valid cmsg header inside `control` (non-null from CMSG_FIRSTHDR / CMSG_NXTHDR).
        let h = unsafe { &*c };
        if h.cmsg_level == libc::IPPROTO_IP && h.cmsg_type == libc::IP_TOS {
            // SAFETY: `c` is a valid cmsg header (see above).
            let data = unsafe { libc::CMSG_DATA(c) };
            // SAFETY: an IP_TOS cmsg carries at least one byte of data at `data`.
            tos = Some(unsafe { *data });
        }
        // SAFETY: as for CMSG_FIRSTHDR.
        c = unsafe { libc::CMSG_NXTHDR(&raw const msg, c) };
    }
    Some((n, ip, tos))
}

/// UDP receive thread (whole app life): AUDIO datagrams on port 7878 -> the current stream's playout.
pub(crate) fn udp_thread(sh: &'static Shared) {
    set_thread_priority(-16);
    let sock = loop {
        match UdpSocket::bind(("0.0.0.0", PORT)) {
            Ok(s) => break s,
            Err(e) => {
                log::error!("bind UDP :{PORT} failed: {e} (retrying)");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };
    set_int_opt(&sock, libc::SOL_SOCKET, libc::SO_RCVBUF, UDP_RCVBUF);
    set_int_opt(&sock, libc::IPPROTO_IP, libc::IP_RECVTOS, 1);
    log::info!("audio: listening on UDP 0.0.0.0:{PORT}");
    let mut buf = [0u8; 2048];
    loop {
        let Some((n, from, tos)) = recv_with_tos(&sock, &mut buf) else { continue };
        let now = now_us();
        let Some(hdr) = buf[..n].first_chunk::<HEADER_LEN>() else { continue };
        let Ok(h) = parse_header(hdr) else { continue };
        if h.typ != T_AUDIO || h.payload_len() != n - HEADER_LEN {
            continue;
        }
        let Some((sid, pkt)) = parse_payload(h.t_us, &buf[HEADER_LEN..n]) else { continue };
        if sh.audio.deliver(from, sid, pkt, now) {
            sh.audio.pkts.fetch_add(1, Ordering::Relaxed);
            sh.audio.dscp.store(tos.map_or(0, |t| (t >> 2) + 1), Ordering::Relaxed);
        }
    }
}

/// Open the `AAudio` output: 48 kHz s16 stereo, low latency, exclusive if the device grants it, else shared.
/// (Usage and content type need API 28; the defaults, media / music, are right here.)
fn open_stream(feed: &Arc<Mutex<Feed>>, io: &Arc<CallbackIo>) -> Result<AudioStream, String> {
    let build = |sharing| {
        let err_io = Arc::clone(io);
        AudioStreamBuilder::new()
            .map(|b| {
                b.data_callback(make_callback(Arc::clone(feed), Arc::clone(io)))
                    .error_callback(Box::new(move |_s: &AudioStream, e| {
                        log::warn!("AAudio error callback: {e:?}");
                        err_io.failed.store(true, Ordering::SeqCst);
                    }))
                    .direction(AudioDirection::Output)
                    .format(AudioFormat::PCM_I16)
                    .channel_count(2)
                    .sample_rate(48_000)
                    .performance_mode(AudioPerformanceMode::LowLatency)
                    .sharing_mode(sharing)
            })
            .and_then(AudioStreamBuilder::open_stream)
    };
    // An exclusive request that cannot get an exclusive MMAP stream does not fail: AAudio falls back to the
    // legacy (AudioTrack) path and reports a shared stream, without ever trying shared MMAP. On some TVs
    // exclusive MMAP always fails that way (the app may not map the HAL's buffer: GET_STREAM_DESCRIPTION
    // returns FAILED_TRANSACTION in logcat) while shared MMAP, whose buffer the AAudio service makes, works.
    // So anything but a granted exclusive stream is closed and the stream is opened again as shared.
    match build(AudioSharingMode::Exclusive) {
        Ok(s) if s.sharing_mode() == AudioSharingMode::Exclusive => Ok(s),
        Ok(s) => {
            log::info!(
                "AAudio exclusive not granted (got {} burst {}); opening shared",
                mode_name(&s),
                s.frames_per_burst()
            );
            drop(s);
            build(AudioSharingMode::Shared).map_err(|e| format!("{e:?}"))
        }
        Err(e) => {
            log::info!("AAudio exclusive refused ({e:?}); opening shared");
            build(AudioSharingMode::Shared).map_err(|e| format!("{e:?}"))
        }
    }
}

fn mode_name(s: &AudioStream) -> &'static str {
    match s.sharing_mode() {
        AudioSharingMode::Exclusive => "exclusive",
        _ => "shared",
    }
}

/// The shown video's delay, sampled from the decoder's latest display time.
#[derive(Debug)]
struct VideoDelay {
    track: VideoTrack,
    last_disp_us: u64,
}

impl VideoDelay {
    fn new() -> VideoDelay {
        VideoDelay { track: VideoTrack::new(VIDEO_WINDOW, VIDEO_WARMUP), last_disp_us: 0 }
    }

    /// Sample once; returns the delay audio should follow (None without fresh, settled video timing).
    fn sample(&mut self, sh: &Shared) -> Option<i64> {
        let (t, d) = {
            let st = lock(&sh.stats);
            (st.disp_t_us, st.disp_us)
        };
        let now = now_us();
        if d == 0 || now.saturating_sub(d) > VIDEO_STALE_US {
            self.track.clear();
            return None;
        }
        if d != self.last_disp_us {
            self.last_disp_us = d;
            self.track.push(i64::try_from(d).unwrap_or(i64::MAX).saturating_sub(i64::try_from(t).unwrap_or(0)));
        }
        self.track.value()
    }
}

/// When the frame `written` of the stream will be presented (TV µs), from the latest `AAudio` timestamp, or from
/// the frames the device has read when there is none yet.
fn present_us(ts: Option<(i64, i64)>, written: i64) -> u64 {
    let now = now_ns();
    let ns = ts.map_or(now, |(frame, ns)| ns + (written - frame) * 1_000_000_000 / i64::from(RATE));
    u64::try_from(ns.max(now) / 1000).unwrap_or(0)
}

/// The stream's latest (frame position, monotonic ns) as the output thread read it, for the callback. The callback
/// must not call `AAudioStream_getTimestamp` itself (on the legacy path it takes `AudioTrack`'s lock). A seqlock:
/// one writer; the reader never waits (on a torn read it keeps the pair it had).
#[derive(Debug, Default)]
struct SharedTimestamp {
    /// Odd while the writer is between its two stores.
    seq: AtomicU64,
    frame: AtomicI64,
    ns: AtomicI64,
}

impl SharedTimestamp {
    fn publish(&self, frame: i64, ns: i64) {
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.frame.store(frame, Ordering::Release);
        self.ns.store(ns, Ordering::Release);
        self.seq.fetch_add(1, Ordering::AcqRel);
    }

    /// The pair, unless none was published yet or the writer is in the middle of one.
    fn read(&self) -> Option<(i64, i64)> {
        let before = self.seq.load(Ordering::Acquire);
        if before == 0 || before % 2 == 1 {
            return None;
        }
        let pair = (self.frame.load(Ordering::Acquire), self.ns.load(Ordering::Acquire));
        (self.seq.load(Ordering::Acquire) == before).then_some(pair)
    }
}

/// Read the stream's timestamp (or, before the first one, the frames read so far as of now) for the callback.
fn sample_timestamp(stream: &AudioStream, out: &SharedTimestamp) {
    match stream.timestamp(Clockid::Monotonic) {
        Ok(t) => out.publish(t.frame_position, t.time_nanoseconds),
        Err(_) if out.read().is_none() => out.publish(stream.frames_read(), now_ns()),
        Err(_) => {}
    }
}

/// The presentation time of each callback, smoothed. The `AAudio` timestamp estimate jumps by tens of ms from one
/// call to the next on this TV; feeding that straight to the playout made it skip and insert audio (clicks).
/// Callbacks are contiguous, so the next one starts where the last ended: follow that, and pull toward the
/// measured time by 1/16 of the gap per call. A gap over [`PRESENT_RESET_US`] (a real jump) is taken at once.
struct PresentClock {
    next: Option<i64>,
}

const PRESENT_RESET_US: i64 = 120_000;
const PRESENT_GAIN: i64 = 16;

impl PresentClock {
    fn next(&mut self, measured: u64, now: u64, frames: i64) -> u64 {
        let m = i64::try_from(measured).unwrap_or(i64::MAX);
        let at = match self.next {
            Some(n) if (m - n).abs() <= PRESENT_RESET_US => n + (m - n) / PRESENT_GAIN,
            _ => m,
        }
        .max(i64::try_from(now).unwrap_or(0));
        self.next = Some(at + frames * 1_000_000 / i64::from(RATE));
        u64::try_from(at).unwrap_or(0)
    }
}

/// Values the output thread hands to the `AAudio` data callback, and back.
#[derive(Debug, Default)]
struct CallbackIo {
    /// Video delay the audio should follow (µs), `i64::MIN` = none.
    video_us: AtomicI64,
    /// The app is in front (else play silence, keep time).
    visible: AtomicBool,
    /// Last output latency the callback saw (µs).
    out_us: AtomicI64,
    /// The stream reported an error (disconnect): reopen.
    failed: AtomicBool,
    /// The stream's latest timestamp, read by the output thread every [`TIMESTAMP_EVERY`].
    timestamp: SharedTimestamp,
}

/// Output thread of one audio stream. `AAudio` pulls audio through a data callback: the Android docs say a
/// blocking `write()` gets no FAST track (high latency, large bursts) and a callback is needed for low latency.
/// This thread opens the stream, feeds the callback the video delay, tunes the buffer and reports stats.
fn output_thread(sh: &'static Shared, feed: &Arc<Mutex<Feed>>, stop: &AtomicBool) {
    let av_offset_us = i64::from(get_prop_i32("debug.lrt.av_offset_ms", 0)) * 1000;
    while !stop.load(Ordering::SeqCst) {
        let io = Arc::new(CallbackIo::default());
        io.video_us.store(i64::MIN, Ordering::Relaxed);
        io.visible.store(sh.visible(), Ordering::Relaxed);
        let stream = match open_stream(feed, &io) {
            Ok(s) => s,
            Err(e) => {
                log::error!("AAudio open failed: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let burst = stream.frames_per_burst().max(48);
        let size = set_buffer_frames(&stream, burst * BUFFER_BURSTS);
        log::info!(
            "AAudio: {} {} Hz, callback, burst {burst} frames, buffer {size} of {} frames",
            mode_name(&stream),
            stream.sample_rate(),
            stream.buffer_capacity_in_frames()
        );
        lock(&sh.audio.out).mode = mode_name(&stream);
        if let Err(e) = stream.request_start() {
            log::error!("AAudio start failed: {e:?}");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let mut video = VideoDelay::new();
        let (mut tune_at, mut xruns_seen) = (Instant::now(), stream.x_run_count());
        let mut video_at = Instant::now();
        while !stop.load(Ordering::SeqCst) && !io.failed.load(Ordering::SeqCst) {
            sample_timestamp(&stream, &io.timestamp);
            std::thread::sleep(TIMESTAMP_EVERY);
            if video_at.elapsed() < VIDEO_EVERY {
                continue;
            }
            video_at = Instant::now();
            let v = video.sample(sh).map_or(i64::MIN, |v| v + av_offset_us);
            io.video_us.store(v, Ordering::Relaxed);
            io.visible.store(sh.visible(), Ordering::Relaxed);
            let mut o = lock(&sh.audio.out);
            o.out_ms = crate::metrics::us_to_ms(u64::try_from(io.out_us.load(Ordering::Relaxed)).unwrap_or(0));
            o.xruns = stream.x_run_count();
            drop(o);
            if tune_at.elapsed() >= TUNE_EVERY {
                tune_at = Instant::now();
                xruns_seen = tune_buffer(&stream, burst, xruns_seen);
            }
        }
        if io.failed.load(Ordering::SeqCst) {
            log::warn!("AAudio stream error; reopening");
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = stream.request_stop();
    }
}

/// One latency-tuner step: after new underruns, one more burst of buffer (up to [`BUFFER_BURSTS_MAX`]). Returns
/// the underrun count seen now.
fn tune_buffer(stream: &AudioStream, burst: i32, seen: i32) -> i32 {
    let now = stream.x_run_count();
    let size = stream.buffer_size_in_frames();
    if now > seen && size < burst * BUFFER_BURSTS_MAX {
        match set_buffer_frames(stream, size + burst) {
            -1 => log::warn!("AAudio: cannot grow the buffer"),
            n => log::info!("AAudio: {} underruns; buffer {size} -> {n} frames", now - seen),
        }
    }
    now
}

/// The `AAudio` data callback: take the datagrams that came in, then fill `frames` frames of the stream's buffer
/// from the playout, for the time they will be presented. Runs on `AAudio`'s high-priority thread: it never
/// blocks (only `try_lock`s, which nothing else holds while a stream runs) and the playout does not allocate.
fn make_callback(feed: Arc<Mutex<Feed>>, io: Arc<CallbackIo>) -> AudioStreamDataCallback {
    let mut written: i64 = 0;
    let mut ts: Option<(i64, i64)> = None;
    let mut clock = PresentClock { next: None };
    Box::new(move |_stream: &AudioStream, data: *mut std::ffi::c_void, frames: i32| {
        let n = usize::try_from(frames).unwrap_or(0);
        // SAFETY: AAudio hands a buffer of `frames` frames in the stream's format (s16 stereo) for this call.
        let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<i16>(), n * 2) };
        ts = io.timestamp.read().or(ts);
        let now = now_us();
        let at = clock.next(present_us(ts, written), now, i64::from(frames));
        let v = io.video_us.load(Ordering::Relaxed);
        match feed.try_lock() {
            Ok(mut f) => {
                let f = &mut *f;
                while let Ok((pkt, arrive_us)) = f.ingest.pop() {
                    f.playout.push(pkt, arrive_us);
                }
                f.playout.set_video((v != i64::MIN).then_some(v));
                f.playout.pull(out, at, now);
                if let Ok(mut s) = f.snapshot.try_lock() {
                    *s = f.playout.snapshot();
                }
            }
            // Only a previous stream's callback could hold it, for an instant: play silence, keep time.
            Err(TryLockError::WouldBlock) => out.fill(0),
            Err(TryLockError::Poisoned(_)) => panic!("audio feed mutex poisoned"),
        }
        if !io.visible.load(Ordering::Relaxed) {
            out.fill(0); // another app is in front: keep time, stay quiet
        }
        written += i64::from(frames);
        io.out_us.store(i64::try_from(at.saturating_sub(now)).unwrap_or(0), Ordering::Relaxed);
        AudioCallbackResult::Continue
    })
}

// ndk 0.9's `set_buffer_size_in_frames` (and `write`) treat every non-zero result as an error, but AAudio returns
// the frame count there (>= 0) and a negative code only on failure. This turns a positive "error" back into the
// count.

/// `AAudio`'s raw result behind an ndk audio result: the count on success, the negative code on failure.
fn aaudio_count<T: TryInto<i32>>(r: ndk::audio::Result<T>) -> i32 {
    match r {
        Ok(n) => n.try_into().unwrap_or(i32::MAX),
        Err(e) => i32::from(e),
    }
}

/// Set the buffer size in frames. Returns the size `AAudio` actually chose, or -1 on failure.
fn set_buffer_frames(stream: &AudioStream, frames: i32) -> i32 {
    let r = aaudio_count(stream.set_buffer_size_in_frames(frames));
    if r < 0 {
        -1
    } else {
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_timestamp_hands_over_whole_pairs_only() {
        let t = SharedTimestamp::default();
        assert_eq!(t.read(), None, "nothing published yet");
        t.publish(480, 1_000_000);
        assert_eq!(t.read(), Some((480, 1_000_000)));
        // A writer between its two stores: the reader does not wait and gets nothing (keeps its old pair).
        t.seq.fetch_add(1, Ordering::AcqRel);
        t.frame.store(960, Ordering::Release);
        assert_eq!(t.read(), None);
        t.ns.store(2_000_000, Ordering::Release);
        t.seq.fetch_add(1, Ordering::AcqRel);
        assert_eq!(t.read(), Some((960, 2_000_000)));
    }

    #[test]
    fn present_time_follows_the_timestamp() {
        // Frame 0 was presented at t0 (1 s from now); frame 4800 (100 ms later at 48 kHz) is presented 100 ms after.
        let t0 = now_ns() + 1_000_000_000;
        let at = present_us(Some((0, t0)), 4_800);
        let want = u64::try_from((t0 + 100_000_000) / 1000).unwrap();
        assert!(at.abs_diff(want) <= 1, "{at} vs {want}");
        // Without a timestamp: now (never in the past).
        assert!(present_us(None, 0) >= now_us().saturating_sub(1_000));
    }
}
