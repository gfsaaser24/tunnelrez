//! Shared state between the activity (main) thread, the network thread, the decoder thread and the
//! output thread, plus the output thread itself.

use crate::audio::Audio;
use crate::codec::{CodecKind, Decoder, Mode, OutBuf, Output};
use crate::format_keys;
use crate::frame_queue::IdrGate;
use crate::metrics::us_to_ms;
use crate::proto::{encode, T_IDR_REQUEST};
use crate::util::{now_ns, now_us, set_thread_priority};
use ndk::native_window::NativeWindow;
use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// Write half of the current session's socket (shared by the network, decoder, stats and main threads).
pub(crate) type Writer = Arc<Mutex<TcpStream>>;

/// Lock a mutex; poisoning means another thread panicked (the app aborts on panic anyway).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().expect("mutex poisoned")
}

/// Per-second and session counters for STATS.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    // Per-second window (reset by the stats reporter).
    pub(crate) frames_in: u32,
    pub(crate) bytes_in: u64,
    pub(crate) frames_out: u32,
    pub(crate) decode_ms: Vec<f32>,
    /// Frame received (fully read from the socket, network thread) -> released to display.
    pub(crate) lat_ms: Vec<f32>,
    /// Interval between consecutive renders.
    pub(crate) interval_ms: Vec<f32>,
    /// Interval between consecutive FRAME arrivals, stamped on the network thread (network + sender jitter).
    pub(crate) arrival_ms: Vec<f32>,
    /// Time the decoder thread waited for a free decoder input buffer, per frame.
    pub(crate) in_wait_ms: Vec<f32>,
    // Cumulative for the session.
    /// Frames dropped anywhere (queue overflow, waiting for a keyframe, background, late output).
    pub(crate) dropped: u64,
    /// Output frames skipped because a newer one was ready.
    pub(crate) late: u64,
    /// Output buffers with no queued frame left to match (the decoder gave more pictures than it got
    /// frames). Not rendered, not counted in `frames_out`.
    pub(crate) out_unmatched: u64,
    pub(crate) last_render_us: u64,
    pub(crate) last_arrival_us: u64,
    /// Latest values (not reset per second): sender `t_us` of the last frame released to the display, and
    /// the TV `CLOCK_MONOTONIC` time (µs) of that release. 0 = nothing released yet.
    pub(crate) disp_t_us: u64,
    pub(crate) disp_us: u64,
}

/// A frame queued into the decoder, waiting for its output buffer.
#[derive(Debug)]
pub(crate) struct InFlight {
    /// Local receive time (µs), also used as the codec pts.
    pub(crate) pts: u64,
    pub(crate) queued_us: u64,
    /// FRAME header `t_us` (sender clock).
    pub(crate) sender_t_us: u64,
}

/// Process-wide state shared by all threads.
#[derive(Debug)]
pub(crate) struct Shared {
    /// The active decoder. Read lock = use; write lock = create/destroy/own the window exclusively.
    dec: RwLock<Option<Decoder>>,
    pub(crate) window: Mutex<Option<NativeWindow>>,
    pub(crate) stats: Mutex<Stats>,
    pub(crate) inflight: Mutex<VecDeque<InFlight>>,
    /// Set by the output thread on a decoder error; the decoder thread recreates the codec.
    pub(crate) dec_error: AtomicBool,
    /// A sender is connected.
    pub(crate) connected: AtomicBool,
    /// The idle screen should be (re)drawn by the main thread.
    pub(crate) idle_dirty: AtomicBool,
    /// 1 = releaseOutputBufferAtTime(now) (default, measured steadiest), 0 = releaseOutputBuffer(render=true).
    /// From debug.lrt.render.
    pub(crate) render_mode: AtomicU32,
    /// The activity is visible (started, with a window). False while another app owns the screen.
    pub(crate) foreground: AtomicBool,
    /// Set when the activity comes back (or the watchdog fires): the decoder thread drops its decoder state
    /// and waits for a fresh keyframe.
    pub(crate) recover: AtomicBool,
    /// The current session's socket, so other threads can send `IDR_REQUEST`. None when no client.
    pub(crate) writer: Mutex<Option<Writer>>,
    idr: Mutex<IdrGate>,
    /// Set by the stats thread when frames arrive but none are shown for 2 s: the decoder thread treats the
    /// decoder as stalled.
    pub(crate) stall: AtomicBool,
    /// Pictures released to the display, process total (the decoder thread watches it for the first picture).
    frames_shown: AtomicU64,
    /// Output buffers still to log for the current decoder.
    out_log_budget: AtomicU32,
    /// First key-ladder rung for the next decoder, per codec (raised when a young decoder stalls).
    rungs: Mutex<[usize; 3]>,
    /// The CAPS message JSON (decoder list), once it is built.
    pub(crate) caps: OnceLock<String>,
    /// Audio of the current session.
    pub(crate) audio: Audio,
}

/// Output buffers logged after each decoder start.
const OUT_LOG_BUFFERS: u32 = 16;

fn kind_index(kind: CodecKind) -> usize {
    match kind {
        CodecKind::H264 => 0,
        CodecKind::Hevc => 1,
        CodecKind::Av1 => 2,
    }
}

impl Shared {
    pub(crate) fn new() -> Shared {
        Shared {
            dec: RwLock::new(None),
            window: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
            inflight: Mutex::new(VecDeque::new()),
            dec_error: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            idle_dirty: AtomicBool::new(true),
            render_mode: AtomicU32::new(0),
            foreground: AtomicBool::new(true),
            recover: AtomicBool::new(false),
            writer: Mutex::new(None),
            idr: Mutex::new(IdrGate::default()),
            stall: AtomicBool::new(false),
            frames_shown: AtomicU64::new(0),
            out_log_budget: AtomicU32::new(0),
            rungs: Mutex::new([0; 3]),
            caps: OnceLock::new(),
            audio: Audio::default(),
        }
    }

    /// Pictures released to the display so far (process total).
    pub(crate) fn frames_shown(&self) -> u64 {
        self.frames_shown.load(Ordering::SeqCst)
    }

    /// A decoder was just created: log its first output buffers.
    pub(crate) fn decoder_started(&self) {
        self.out_log_budget.store(OUT_LOG_BUFFERS, Ordering::SeqCst);
    }

    /// The key-ladder rung the next `kind` decoder starts at.
    pub(crate) fn rung(&self, kind: CodecKind) -> usize {
        lock(&self.rungs)[kind_index(kind)]
    }

    /// A `kind` decoder on rung `rung` (of `rungs`) stalled soon after it started: the next one starts one rung
    /// lower (fewer optional keys). Returns the new rung, or None if it already was the last one.
    pub(crate) fn escalate(&self, kind: CodecKind, rung: usize, rungs: usize) -> Option<usize> {
        let next = format_keys::next_rung(rung, rungs)?;
        let mut r = lock(&self.rungs);
        let slot = &mut r[kind_index(kind)];
        *slot = (*slot).max(next);
        Some(*slot)
    }

    pub(crate) fn dec_read(&self) -> RwLockReadGuard<'_, Option<Decoder>> {
        self.dec.read().expect("decoder lock poisoned")
    }

    pub(crate) fn dec_write(&self) -> RwLockWriteGuard<'_, Option<Decoder>> {
        self.dec.write().expect("decoder lock poisoned")
    }

    /// Ask the sender for a keyframe. At most one request is outstanding until a keyframe arrives (see
    /// [`IdrGate`]). Never call while holding `dec`.
    pub(crate) fn request_idr(&self, why: &str) -> bool {
        let go = lock(&self.idr).request(Instant::now());
        go && self.send_idr(why)
    }

    /// Ask for a keyframe even if one is outstanding (still rate limited). For a fresh decoder that needs
    /// one now (app back in front, watchdog).
    pub(crate) fn force_idr(&self, why: &str) -> bool {
        let go = lock(&self.idr).force(Instant::now());
        go && self.send_idr(why)
    }

    /// A keyframe arrived from the sender (network thread).
    pub(crate) fn keyframe_arrived(&self) {
        lock(&self.idr).keyframe_arrived();
    }

    /// IDR requests sent since the previous call.
    pub(crate) fn take_idr_sent(&self) -> u32 {
        lock(&self.idr).take_sent()
    }

    fn send_idr(&self, why: &str) -> bool {
        let Some(w) = lock(&self.writer).clone() else {
            return false;
        };
        log::info!("IDR_REQUEST ({why})");
        let msg = encode(T_IDR_REQUEST, 0, now_us(), &[]);
        let ok = lock(&w).write_all(&msg).is_ok();
        ok
    }

    /// Stop and release the decoder (frees the window for other producers).
    pub(crate) fn destroy_decoder(&self) {
        let mut g = self.dec_write();
        if g.take().is_some() {
            lock(&self.inflight).clear();
            self.idle_dirty.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn has_decoder(&self) -> bool {
        self.dec_read().is_some()
    }

    /// The activity is in front and has a window.
    pub(crate) fn visible(&self) -> bool {
        self.foreground.load(Ordering::SeqCst) && lock(&self.window).is_some()
    }

    pub(crate) fn reset_stats(&self) {
        *lock(&self.stats) = Stats::default();
        lock(&self.idr).take_sent();
    }
}

/// Output thread: dequeue decoded buffers with short timeouts; when more than one is ready, render only
/// the newest and release the rest unrendered, so no display queue can build up.
pub(crate) fn output_thread(sh: &'static Shared) {
    set_thread_priority(-16);
    let mut pending: Vec<OutBuf> = Vec::with_capacity(8);
    loop {
        let g = sh.dec_read();
        let Some(dec) = g.as_ref() else {
            drop(g);
            std::thread::sleep(Duration::from_millis(3));
            continue;
        };
        pending.clear();
        let mut timeout = 4_000i64; // block for the first one only
        let mut failed = false;
        loop {
            match dec.dequeue_output(timeout) {
                Output::Buffer(b) if b.is_picture() => {
                    pending.push(b);
                    timeout = 0;
                }
                Output::Buffer(b) => {
                    // Empty, codec-config or end-of-stream buffer: not a picture.
                    log_output(sh, b, "not a picture");
                    failed |= !dec.discard(b.idx);
                    timeout = 0;
                }
                Output::TryAgain => break,
                Output::FormatChanged => {
                    log::info!("output format: {}", dec.output_format());
                    timeout = 0;
                }
                Output::BuffersChanged => timeout = 0,
                Output::Error(e) => {
                    log::error!("dequeueOutputBuffer error {e}");
                    failed = true;
                    break;
                }
            }
        }
        let shown = if pending.is_empty() { Shown::Nothing } else { render(sh, dec, &pending) };
        failed |= shown == Shown::Failed;
        drop(g);
        if failed {
            sh.dec_error.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
        } else if shown == Shown::OnlyUnmatched {
            // The decoder hands back pictures it got no frame for (the MTK AV1 decoder does this when fed
            // temporal delimiters). They go straight back to it: do not spin on them.
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Log one output buffer while the per-decoder budget lasts.
fn log_output(sh: &Shared, b: OutBuf, what: &str) {
    let left = sh.out_log_budget.load(Ordering::Relaxed);
    if left > 0 {
        sh.out_log_budget.store(left - 1, Ordering::Relaxed);
        log::info!(
            "output #{}: idx={} size={} flags={:#x} pts={} {what}",
            OUT_LOG_BUFFERS - left + 1,
            b.idx,
            b.size,
            b.flags,
            b.pts_us
        );
    }
}

/// What [`render`] did with a batch of output buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Nothing,
    /// The newest buffer that matches a queued frame was released to the display.
    Picture,
    /// No buffer matched a queued frame; all were released unrendered.
    OnlyUnmatched,
    /// Releasing a buffer failed.
    Failed,
}

/// Match `bufs` (oldest first) to the queued frames, render the newest matched one, release the rest
/// unrendered, update stats. Only a rendered picture that matches a queued frame counts in `fps_out`.
fn render(sh: &Shared, dec: &Decoder, bufs: &[OutBuf]) -> Shown {
    debug_assert_eq!(dec.mode, Mode::Buffers, "tunneled decoders return no output buffers");
    let now = now_us();
    // Match outputs to inputs in FIFO order (no B-frames: 1 in -> 1 out, in order). The MTK HEVC
    // decoder stamps outputs with the newest input's pts, so pts matching is not reliable here.
    let matched: Vec<Option<InFlight>> = {
        let mut q = lock(&sh.inflight);
        bufs.iter().map(|_| q.pop_front()).collect()
    };
    for (b, f) in bufs.iter().zip(&matched) {
        log_output(sh, *b, if f.is_some() { "matched" } else { "UNMATCHED (no frame queued)" });
    }
    let show = matched.iter().rposition(Option::is_some);
    let mut ok = true;
    for (i, b) in bufs.iter().enumerate() {
        if Some(i) != show {
            ok &= dec.discard(b.idx);
        }
    }
    let decode_ms: Vec<f32> = matched.iter().flatten().map(|f| us_to_ms(now.saturating_sub(f.queued_us))).collect();
    let count = |n: usize| u64::try_from(n).expect("usize fits in u64");
    let unmatched = count(matched.iter().filter(|f| f.is_none()).count());
    let Some(show) = show else {
        lock(&sh.stats).out_unmatched += unmatched;
        return if ok { Shown::OnlyUnmatched } else { Shown::Failed };
    };
    let newest = bufs[show].idx;
    let rendered =
        if sh.render_mode.load(Ordering::Relaxed) == 1 { dec.render_at(newest, now_ns()) } else { dec.render(newest) };
    let t = now_us();
    let mut s = lock(&sh.stats);
    // Matched pictures older than the one shown: decoded in time, never displayed.
    let skipped = count(matched[..show].iter().filter(|f| f.is_some()).count());
    s.late += skipped;
    s.dropped += skipped;
    s.out_unmatched += unmatched;
    s.decode_ms.extend_from_slice(&decode_ms);
    if !rendered {
        return Shown::Failed;
    }
    s.frames_out += 1;
    sh.frames_shown.fetch_add(1, Ordering::SeqCst);
    if let Some(f) = &matched[show] {
        // pts is the local receive time (µs, CLOCK_MONOTONIC).
        s.lat_ms.push(us_to_ms(t.saturating_sub(f.pts)));
        s.disp_t_us = f.sender_t_us;
        s.disp_us = t;
    }
    if s.last_render_us != 0 {
        let iv = us_to_ms(t.saturating_sub(s.last_render_us));
        s.interval_ms.push(iv);
    }
    s.last_render_us = t;
    if ok {
        Shown::Picture
    } else {
        Shown::Failed
    }
}
