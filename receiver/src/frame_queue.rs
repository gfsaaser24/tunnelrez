//! Hand-off from the network thread to the decoder thread, and the keyframe-request gate.
//!
//! The network thread only reads the socket and pushes here; it never waits for the decoder. The queue
//! holds at most `cap` frames. When a frame arrives and the queue is full, the queued frames are dropped:
//! the decoder is behind, and older frames would only add latency. After such a drop, P-frames cannot be
//! decoded, so the queue drops them until a keyframe comes, and the caller asks the sender for one IDR.
//! HELLO / CONFIG items are kept in order, but a newer one replaces an older one of the same kind when no frame
//! is queued between them, so they are bounded too (see `drop_superseded`).
//!
//! While a decoder starts (it is created, and its first picture is not shown yet), the queue may hold up to
//! `start_cap` frames instead of `cap`: creating a hardware decoder takes ~80 ms, during which 5 frames arrive.
//! After the first picture the decoder drains the backlog faster than real time; the smaller cap applies again
//! once the queue is down to it (see [`FrameQueue::end_startup`]).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Default queue capacity in frames (3 = 50 ms at 60 fps).
pub(crate) const DEFAULT_CAP: usize = 3;
/// Largest capacity accepted from `debug.lrt.rxq`.
pub(crate) const MAX_CAP: usize = 16;
/// Capacity while a decoder starts (12 = 200 ms at 60 fps).
pub(crate) const START_CAP: usize = 12;

/// One video frame as read from the socket.
#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) key: bool,
    /// FRAME header `t_us` (sender clock, µs).
    pub(crate) sender_t_us: u64,
    /// TV `CLOCK_MONOTONIC` µs when the last payload byte was read (network thread).
    pub(crate) recv_us: u64,
    pub(crate) data: Vec<u8>,
}

/// What the network thread hands to the decoder thread, in stream order.
#[derive(Debug)]
pub(crate) enum Item {
    Hello(Vec<u8>),
    Config(Vec<u8>),
    Frame(Frame),
}

/// Result of [`FrameQueue::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Push {
    /// Queued.
    Queued,
    /// Queue was full with a keyframe arriving: the older frames were dropped, the keyframe was queued.
    /// No IDR request is needed.
    Flushed { dropped: usize },
    /// Queue was full with a P-frame arriving: the queued frames and this one were dropped. The queue now
    /// drops P-frames until a keyframe. The caller should request an IDR.
    Overflow { dropped: usize },
    /// Dropped: still waiting for a keyframe after an overflow.
    WaitingForKey,
    /// Dropped: the queue is closed.
    Closed,
}

#[derive(Debug, Default)]
struct State {
    items: VecDeque<Item>,
    /// Number of `Item::Frame` in `items`.
    frames: usize,
    waiting_key: bool,
    closed: bool,
    /// Largest `frames` seen since the last [`FrameQueue::take_window`].
    max_frames: usize,
    /// Frames dropped by the queue (session total).
    dropped: u64,
    /// Which capacity applies now.
    phase: Phase,
}

/// Startup state of the queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    /// A decoder is starting: `start_cap` applies.
    #[default]
    Starting,
    /// The first picture is shown, but the backlog is above `cap`: `start_cap` still applies, so draining it
    /// cannot overflow the queue.
    Draining,
    /// `cap` applies.
    Running,
}

/// Per-window queue numbers for STATS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    /// Frames queued right now.
    pub(crate) depth: usize,
    /// Largest depth since the previous window.
    pub(crate) max_depth: usize,
    /// Frames dropped by the queue, session total.
    pub(crate) dropped: u64,
    /// The capacity that applies now (`start_cap` while a decoder starts, else `cap`).
    pub(crate) limit: usize,
}

/// Bounded multi-item queue, one producer (network thread), one consumer (decoder thread).
#[derive(Debug)]
pub(crate) struct FrameQueue {
    cap: usize,
    start_cap: usize,
    state: Mutex<State>,
    ready: Condvar,
}

impl FrameQueue {
    /// A queue holding at most `cap` frames (clamped to 1..=[`MAX_CAP`]), and up to `start_cap` (at least
    /// `cap`) while a decoder starts. It starts in the startup phase. HELLO / CONFIG items do not count.
    pub(crate) fn new(cap: usize, start_cap: usize) -> FrameQueue {
        let cap = cap.clamp(1, MAX_CAP);
        FrameQueue {
            cap,
            start_cap: start_cap.clamp(cap, MAX_CAP),
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        }
    }

    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    pub(crate) fn start_cap(&self) -> usize {
        self.start_cap
    }

    /// A (new) decoder is starting: allow `start_cap` frames until [`FrameQueue::end_startup`].
    pub(crate) fn begin_startup(&self) {
        self.lock().phase = Phase::Starting;
    }

    /// The decoder showed its first picture: go back to `cap` once the backlog is below it (so the next frame
    /// fits).
    pub(crate) fn end_startup(&self) {
        let mut s = self.lock();
        if s.phase == Phase::Starting {
            s.phase = if s.frames >= self.cap { Phase::Draining } else { Phase::Running };
        }
    }

    /// True until the first picture of the current decoder was shown and the backlog drained.
    #[cfg(test)]
    fn is_starting(&self) -> bool {
        self.lock().phase != Phase::Running
    }

    /// The capacity that applies now.
    fn limit(&self, s: &State) -> usize {
        match s.phase {
            Phase::Starting | Phase::Draining => self.start_cap,
            Phase::Running => self.cap,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("frame queue lock poisoned")
    }

    /// Add an item. Never blocks on the consumer.
    pub(crate) fn push(&self, item: Item) -> Push {
        let mut s = self.lock();
        if s.closed {
            return Push::Closed;
        }
        let limit = self.limit(&s);
        let res = match &item {
            Item::Frame(f) if f.key => {
                s.waiting_key = false;
                if s.frames >= limit {
                    Push::Flushed { dropped: drop_frames(&mut s) }
                } else {
                    Push::Queued
                }
            }
            Item::Frame(_) if s.waiting_key => {
                s.dropped += 1;
                return Push::WaitingForKey;
            }
            Item::Frame(_) if s.frames >= limit => {
                let dropped = drop_frames(&mut s) + 1;
                s.dropped += 1;
                s.waiting_key = true;
                return Push::Overflow { dropped };
            }
            Item::Hello(_) | Item::Config(_) => {
                drop_superseded(&mut s.items, &item);
                Push::Queued
            }
            Item::Frame(_) => Push::Queued,
        };
        if matches!(item, Item::Frame(_)) {
            s.frames += 1;
            s.max_frames = s.max_frames.max(s.frames);
        }
        s.items.push_back(item);
        drop(s);
        self.ready.notify_one();
        res
    }

    /// Take the oldest item, waiting up to `timeout`. None on timeout or when closed and empty.
    pub(crate) fn pop(&self, timeout: Duration) -> Option<Item> {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        loop {
            if let Some(item) = s.items.pop_front() {
                if matches!(item, Item::Frame(_)) {
                    s.frames -= 1;
                    if s.phase == Phase::Draining && s.frames < self.cap {
                        s.phase = Phase::Running;
                    }
                }
                return Some(item);
            }
            if s.closed {
                return None;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            s = self.ready.wait_timeout(s, left).expect("frame queue lock poisoned").0;
        }
    }

    /// Stop accepting items and wake the consumer. Items already queued can still be popped.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Current depth, the largest depth since the last call, and the dropped total; starts a new window.
    pub(crate) fn take_window(&self) -> Window {
        let mut s = self.lock();
        let w = Window { depth: s.frames, max_depth: s.max_frames, dropped: s.dropped, limit: self.limit(&s) };
        s.max_frames = s.frames;
        w
    }
}

/// Remove every queued frame. Of the HELLO / CONFIG items, only the newest of each kind stays (in order).
/// Returns how many frames were removed.
fn drop_frames(s: &mut State) -> usize {
    let n = s.frames;
    s.items.retain(|i| !matches!(i, Item::Frame(_)));
    keep_newest_controls(&mut s.items);
    s.frames = 0;
    s.dropped += u64::try_from(n).expect("usize fits in u64");
    n
}

/// A new HELLO or CONFIG replaces any of the same kind queued after the last frame. With no frame in between,
/// the older one can never matter: the decoder thread only keeps the newest HELLO and CONFIG, and the sender
/// resends CONFIG before every keyframe. This bounds the control items to at most one HELLO and one CONFIG
/// per gap between queued frames, so at most `2 * (cap + 1)` in total, whatever the sender does.
fn drop_superseded(items: &mut VecDeque<Item>, new: &Item) {
    let tail = items.iter().rposition(|i| matches!(i, Item::Frame(_))).map_or(0, |p| p + 1);
    let kind = std::mem::discriminant(new);
    let mut idx = tail;
    while idx < items.len() {
        if std::mem::discriminant(&items[idx]) == kind {
            items.remove(idx);
        } else {
            idx += 1;
        }
    }
}

/// `items` holds no frames: keep only the newest HELLO and the newest CONFIG, in their order.
fn keep_newest_controls(items: &mut VecDeque<Item>) {
    let last_hello = items.iter().rposition(|i| matches!(i, Item::Hello(_)));
    let last_config = items.iter().rposition(|i| matches!(i, Item::Config(_)));
    let mut idx = 0usize;
    items.retain(|_| {
        let keep = Some(idx) == last_hello || Some(idx) == last_config;
        idx += 1;
        keep
    });
}

/// Shortest time between two `IDR_REQUEST`s.
pub(crate) const IDR_MIN_INTERVAL: Duration = Duration::from_millis(300);
/// After a request, do not ask again for this long unless a keyframe arrived in between. A 4K keyframe
/// takes one round trip plus its own send time; asking again sooner only makes more big keyframes.
pub(crate) const IDR_RETRY_AFTER: Duration = Duration::from_secs(1);

/// Decides when an `IDR_REQUEST` may go out: at most one outstanding request, rate limited.
#[derive(Debug, Default)]
pub(crate) struct IdrGate {
    last_sent: Option<Instant>,
    /// Set when a request is sent, cleared when any keyframe arrives.
    pending_since: Option<Instant>,
    /// Requests sent since the last [`IdrGate::take_sent`].
    sent: u32,
}

impl IdrGate {
    /// Ask for a keyframe. True if a request should be sent now: no request is outstanding (or the
    /// outstanding one is older than [`IDR_RETRY_AFTER`]) and the last one is older than
    /// [`IDR_MIN_INTERVAL`].
    pub(crate) fn request(&mut self, now: Instant) -> bool {
        if self.pending_since.is_some_and(|p| now.saturating_duration_since(p) < IDR_RETRY_AFTER) {
            return false;
        }
        self.force(now)
    }

    /// Like [`IdrGate::request`] but ignores an outstanding request (only the minimum interval applies).
    /// For a fresh decoder that needs a keyframe now (app back in front, watchdog).
    pub(crate) fn force(&mut self, now: Instant) -> bool {
        if self.last_sent.is_some_and(|t| now.saturating_duration_since(t) < IDR_MIN_INTERVAL) {
            return false;
        }
        self.last_sent = Some(now);
        self.pending_since = Some(now);
        self.sent += 1;
        true
    }

    /// A keyframe arrived: the outstanding request (if any) is answered.
    pub(crate) fn keyframe_arrived(&mut self) {
        self.pending_since = None;
    }

    /// Requests sent since the previous call.
    pub(crate) fn take_sent(&mut self) -> u32 {
        std::mem::take(&mut self.sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Kind {
        Key,
        Delta,
    }
    use Kind::{Delta as P, Key as K};

    fn frame(kind: Kind, n: u8) -> Item {
        Item::Frame(Frame { key: matches!(kind, K), sender_t_us: u64::from(n), recv_us: 0, data: vec![n] })
    }

    fn popped_ids(q: &FrameQueue) -> Vec<String> {
        let mut v = Vec::new();
        while let Some(i) = q.pop(Duration::ZERO) {
            v.push(match i {
                Item::Hello(_) => "H".to_string(),
                Item::Config(_) => "C".to_string(),
                Item::Frame(f) => format!("{}{}", if f.key { "K" } else { "P" }, f.data[0]),
            });
        }
        v
    }

    #[test]
    fn queues_in_order_up_to_cap() {
        let q = FrameQueue::new(3, 3);
        assert_eq!(q.push(Item::Hello(vec![])), Push::Queued);
        assert_eq!(q.push(Item::Config(vec![])), Push::Queued);
        assert_eq!(q.push(frame(K, 1)), Push::Queued);
        assert_eq!(q.push(frame(P, 2)), Push::Queued);
        assert_eq!(q.push(frame(P, 3)), Push::Queued);
        assert_eq!(q.take_window(), Window { depth: 3, max_depth: 3, dropped: 0, limit: 3 });
        assert_eq!(popped_ids(&q), ["H", "C", "K1", "P2", "P3"]);
    }

    #[test]
    fn overflow_drops_all_then_waits_for_keyframe() {
        let q = FrameQueue::new(2, 2);
        q.push(frame(K, 1));
        q.push(frame(P, 2));
        // Full: this P-frame overflows. Queued frames + this one are dropped.
        assert_eq!(q.push(frame(P, 3)), Push::Overflow { dropped: 3 });
        assert_eq!(q.push(frame(P, 4)), Push::WaitingForKey);
        assert_eq!(q.push(frame(P, 5)), Push::WaitingForKey);
        // CONFIG is never dropped; the keyframe ends the wait.
        assert_eq!(q.push(Item::Config(vec![])), Push::Queued);
        assert_eq!(q.push(frame(K, 6)), Push::Queued);
        assert_eq!(q.push(frame(P, 7)), Push::Queued);
        let w = q.take_window();
        assert_eq!(w.dropped, 5);
        assert_eq!(w.depth, 2);
        assert_eq!(popped_ids(&q), ["C", "K6", "P7"]);
    }

    #[test]
    fn full_queue_and_keyframe_flushes_without_waiting() {
        let q = FrameQueue::new(2, 2);
        q.push(Item::Config(vec![]));
        q.push(frame(P, 1));
        q.push(frame(P, 2));
        assert_eq!(q.push(frame(K, 3)), Push::Flushed { dropped: 2 });
        assert_eq!(q.push(frame(P, 4)), Push::Queued);
        assert_eq!(popped_ids(&q), ["C", "K3", "P4"]);
        assert_eq!(q.take_window().dropped, 2);
    }

    #[test]
    fn consumer_keeping_up_never_drops() {
        let q = FrameQueue::new(2, 2);
        for n in 0..100u8 {
            assert_eq!(q.push(frame(if n == 0 { K } else { P }, n)), Push::Queued);
            assert!(q.pop(Duration::ZERO).is_some());
        }
        assert_eq!(q.take_window(), Window { depth: 0, max_depth: 1, dropped: 0, limit: 2 });
    }

    #[test]
    fn window_max_depth_resets_to_current() {
        let q = FrameQueue::new(3, 3);
        q.push(frame(K, 1));
        q.push(frame(P, 2));
        q.pop(Duration::ZERO);
        assert_eq!(q.take_window().max_depth, 2);
        assert_eq!(q.take_window().max_depth, 1);
    }

    #[test]
    fn control_items_replace_older_ones_after_the_last_frame() {
        let q = FrameQueue::new(3, 3);
        for _ in 0..128 {
            assert_eq!(q.push(Item::Config(vec![0; 64])), Push::Queued);
        }
        assert_eq!(popped_ids(&q), ["C"]);

        q.push(frame(K, 1));
        q.push(Item::Config(vec![]));
        q.push(Item::Config(vec![]));
        q.push(frame(P, 2));
        q.push(Item::Hello(vec![]));
        q.push(Item::Config(vec![1]));
        q.push(Item::Hello(vec![2]));
        q.push(Item::Config(vec![3]));
        // Before a frame, a control item stays: it applies to the frames after it.
        assert_eq!(popped_ids(&q), ["K1", "C", "P2", "H", "C"]);
    }

    #[test]
    fn overflow_keeps_only_newest_hello_and_config() {
        let q = FrameQueue::new(2, 2);
        q.push(Item::Hello(vec![1]));
        q.push(Item::Config(vec![1]));
        q.push(frame(K, 1));
        q.push(Item::Hello(vec![2]));
        q.push(frame(P, 2));
        q.push(Item::Config(vec![2]));
        assert_eq!(q.push(frame(P, 3)), Push::Overflow { dropped: 3 });
        let mut got = Vec::new();
        while let Some(i) = q.pop(Duration::ZERO) {
            got.push(match i {
                Item::Hello(p) => format!("H{}", p[0]),
                Item::Config(p) => format!("C{}", p[0]),
                Item::Frame(_) => "F".into(),
            });
        }
        assert_eq!(got, ["H2", "C2"]);
    }

    /// Whatever mix arrives and however slow the consumer, control items stay within `2 * (cap + 1)`.
    #[test]
    fn control_items_are_bounded() {
        let cap = 3;
        let q = FrameQueue::new(cap, cap);
        let mut seed = 0x2545_f491_u32;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let item = match seed % 5 {
                0 => Item::Hello(vec![]),
                1 => Item::Config(vec![]),
                2 => frame(K, 0),
                _ => frame(P, 0),
            };
            q.push(item);
            if seed.is_multiple_of(7) {
                q.pop(Duration::ZERO);
            }
            let s = q.lock();
            let controls = s.items.len() - s.frames;
            assert!(s.frames <= cap);
            assert!(controls <= 2 * (cap + 1), "{controls} control items");
        }
    }

    #[test]
    fn startup_holds_more_frames_until_first_picture() {
        let q = FrameQueue::new(3, 12);
        assert!(q.is_starting());
        // The decoder is being created: 12 frames fit.
        for n in 0..12u8 {
            assert_eq!(q.push(frame(if n == 0 { K } else { P }, n)), Push::Queued);
        }
        // The 13th overflows, as the normal cap would.
        assert_eq!(q.push(frame(P, 12)), Push::Overflow { dropped: 13 });
    }

    #[test]
    fn startup_ends_only_after_the_backlog_drained() {
        let q = FrameQueue::new(3, 12);
        for n in 0..8u8 {
            assert_eq!(q.push(frame(if n == 0 { K } else { P }, n)), Push::Queued);
        }
        q.pop(Duration::ZERO);
        // First picture shown with 7 frames queued: no drop while they drain (decoder faster than the stream).
        q.end_startup();
        assert!(q.is_starting());
        for n in 8..14u8 {
            q.pop(Duration::ZERO);
            q.pop(Duration::ZERO);
            assert_eq!(q.push(frame(P, n)), Push::Queued, "frame {n}");
        }
        // Below the normal cap: it applies again.
        assert!(!q.is_starting());
        assert_eq!(q.take_window().dropped, 0);
        while q.take_window().depth < 3 {
            q.push(frame(P, 0));
        }
        assert!(matches!(q.push(frame(P, 0)), Push::Overflow { .. }));
    }

    #[test]
    fn end_startup_with_a_short_queue_switches_at_once() {
        let q = FrameQueue::new(3, 12);
        q.push(frame(K, 1));
        q.end_startup();
        assert!(!q.is_starting());
        q.push(frame(P, 2));
        q.push(frame(P, 3));
        assert!(matches!(q.push(frame(P, 4)), Push::Overflow { .. }));
        // A new decoder (e.g. after recovery) gets the startup room again.
        q.push(frame(K, 5));
        q.begin_startup();
        for n in 6..16u8 {
            assert_eq!(q.push(frame(P, n)), Push::Queued);
        }
    }

    #[test]
    fn start_cap_is_at_least_cap() {
        assert_eq!(FrameQueue::new(5, 2).start_cap(), 5);
        assert_eq!(FrameQueue::new(3, 100).start_cap(), MAX_CAP);
    }

    #[test]
    fn cap_is_clamped() {
        assert_eq!(FrameQueue::new(0, 0).cap(), 1);
        assert_eq!(FrameQueue::new(1000, 1000).cap(), MAX_CAP);
    }

    #[test]
    fn close_wakes_consumer_and_rejects_pushes() {
        let q = std::sync::Arc::new(FrameQueue::new(2, 2));
        let q2 = std::sync::Arc::clone(&q);
        let t = std::thread::spawn(move || q2.pop(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(20));
        q.close();
        assert!(t.join().unwrap().is_none());
        assert!(q.is_closed());
        assert_eq!(q.push(frame(K, 1)), Push::Closed);
    }

    #[test]
    fn pop_wakes_on_push() {
        let q = std::sync::Arc::new(FrameQueue::new(2, 2));
        let q2 = std::sync::Arc::clone(&q);
        let t = std::thread::spawn(move || q2.pop(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(20));
        q.push(frame(K, 9));
        assert!(matches!(t.join().unwrap(), Some(Item::Frame(f)) if f.data == [9]));
    }

    #[test]
    fn idr_gate_one_outstanding_request() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut g = IdrGate::default();
        assert!(g.request(ms(0)));
        // Outstanding: no repeat before the retry time, even after the min interval.
        assert!(!g.request(ms(100)));
        assert!(!g.request(ms(500)));
        assert!(!g.request(ms(999)));
        // Retry when no keyframe came.
        assert!(g.request(ms(1000)));
        // A keyframe answers it: the next request may go out once the min interval passed.
        g.keyframe_arrived();
        assert!(!g.request(ms(1200)));
        assert!(g.request(ms(1300)));
        assert_eq!(g.take_sent(), 3);
        assert_eq!(g.take_sent(), 0);
    }

    #[test]
    fn idr_gate_force_ignores_outstanding_but_not_min_interval() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut g = IdrGate::default();
        assert!(g.request(ms(0)));
        assert!(!g.force(ms(100)));
        assert!(g.force(ms(300)));
        assert_eq!(g.take_sent(), 2);
    }

    /// Simulates a decoder slower than the stream: drops happen in bursts, each burst asks for exactly one
    /// keyframe, and the queue never grows past its cap.
    #[test]
    fn slow_consumer_bursts_ask_one_idr_each() {
        let q = FrameQueue::new(3, 3);
        let mut gate = IdrGate::default();
        let t0 = Instant::now();
        let mut idr_sent = 0u32;
        let mut key_due: Option<u64> = None;
        for tick in 0..600u64 {
            // 60 fps for 10 s; the sender answers an IDR request 100 ms later with a keyframe.
            let now = t0 + Duration::from_micros(tick * 16_667);
            let key = tick == 0 || key_due.is_some_and(|d| tick >= d);
            if key {
                key_due = None;
                gate.keyframe_arrived();
            }
            let n = u8::try_from(tick % 256).unwrap();
            match q.push(frame(if key { K } else { P }, n)) {
                Push::Overflow { .. } | Push::WaitingForKey => {
                    if gate.request(now) {
                        idr_sent += 1;
                        key_due = Some(tick + 6);
                    }
                }
                Push::Queued | Push::Flushed { .. } | Push::Closed => {}
            }
            // The decoder takes 1 frame every 2 ticks (30 fps).
            if tick % 2 == 0 {
                q.pop(Duration::ZERO);
            }
            assert!(q.take_window().depth <= 3);
        }
        let dropped = q.take_window().dropped;
        assert!(dropped > 0, "a 30 fps consumer must drop");
        // One request per overflow burst, each answered before the next: far fewer than one per drop.
        assert!(idr_sent > 0 && u64::from(idr_sent) * 3 < dropped, "idr {idr_sent}, dropped {dropped}");
        assert_eq!(u64::from(idr_sent), u64::from(gate.take_sent()));
    }
}
