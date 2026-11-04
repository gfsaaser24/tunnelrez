//! Audio playout: jitter buffer, play delay (jitter target + video sync) and resync (PROTOCOL.md, "Audio").
//!
//! Clock driven, never fill driven: the frame captured at sender time `t` is presented at TV time `t + P`. `P`
//! comes from the measured network delay, not from how much audio is queued, so a Wi-Fi burst can never grow
//! the buffer: audio that arrives too late is dropped, missing audio plays as silence. Pure logic on the
//! caller's clock, so it is unit tested on the host.

use std::collections::VecDeque;

/// Wire sample rate.
pub(crate) const RATE: u32 = 48_000;
/// Frames per AUDIO datagram (5 ms).
pub(crate) const PACKET_FRAMES: usize = 240;
const PACKET_FRAMES_U64: u64 = 240;
/// Delay samples kept for `d_min` and the jitter estimate.
const WINDOW_US: u64 = 2_000_000;
/// Jitter target: start, limits, margin over the p95 jitter, and how fast it may shrink per update.
const TARGET_START_US: i64 = 30_000;
const TARGET_MIN_US: i64 = 20_000;
const TARGET_MAX_US: i64 = 80_000;
const TARGET_MARGIN_US: i64 = 3_000;
const TARGET_DECAY_US: i64 = 500;
/// After the target rose, it holds for this long before it may shrink. On a still screen the link is quiet and
/// the p95 falls to the floor within seconds; when the picture starts moving, big video frames delay audio again
/// and a target already at the floor turned into underruns, drops and a silence insert. Holding the recent peak
/// keeps the margin through the short still periods between bursts of motion.
const TARGET_HOLD_US: u64 = 10_000_000;
/// How often the play delay is recomputed.
const UPDATE_US: u64 = 100_000;
/// Video sync may add at most this on top of the audio-only delay.
const VIDEO_MAX_EXTRA_US: i64 = 300_000;
/// A new play delay is only applied when it moves more than this.
const DELAY_HYSTERESIS_US: i64 = 3_000;
/// The video delay the audio follows only moves when the measured one moved more than this. Video display
/// times jitter by a few ms; following each change would slip audio frames all the time, while people notice
/// A/V offsets only from about 15 ms (audio early) or 45 ms (audio late).
const VIDEO_HYSTERESIS_US: i64 = 8_000;
/// Position errors up to this (frames, 1 ms) are left alone; above it the playout changes its rate until the
/// error is back under [`ADJUST_STOP`] (0.25 ms).
const ADJUST_START: i64 = 48;
const ADJUST_STOP: i64 = 12;
/// Rate change while adjusting: the error is taken out over about [`ADJUST_SECONDS`], at most 0.5 % fast or
/// slow (5 ms of audio per second; a pitch change far below what people hear). Linear interpolation between
/// frames, so there is no jump in the waveform (dropping or repeating a frame clicks on tones).
const ADJUST_MAX: f64 = 0.005;
const ADJUST_SECONDS: f64 = 1.0;
/// Position errors above this (frames, 20 ms) resync at once instead.
const RESYNC_FRAMES: i64 = 960;
/// Silence-insert resyncs run until the error is within this (frames, 1 ms).
const SLIP_DEADBAND: i64 = 48;
/// The output lead holds its largest value for this long before it may shrink (like the jitter target).
const LEAD_HOLD_US: u64 = 10_000_000;
/// At most this much audio is held (1 s): datagram slots in the ring; a newer datagram replaces the one in its
/// slot.
const MAX_PACKETS: usize = 200;
/// Room for delay samples (2 s of datagrams is 400) and the jitter scratch, reserved up front so the playout
/// does not allocate while it runs on the audio callback.
const WINDOW_CAP: usize = 4_096;
/// No datagram for this long: forget the stream position and start over when audio returns.
const IDLE_RESET_US: u64 = 500_000;

/// One received AUDIO datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Packet {
    /// Index of the first frame since `AUDIO_START`.
    pub(crate) pos: u64,
    /// Sender clock (µs) at the capture of the first frame.
    pub(crate) t_us: u64,
    /// [`PACKET_FRAMES`] interleaved stereo frames.
    pub(crate) pcm: Vec<i16>,
}

/// Parse an AUDIO datagram payload (after the 20-byte header): `sid`, then the packet.
pub(crate) fn parse_payload(t_us: u64, p: &[u8]) -> Option<(u32, Packet)> {
    let (sid, rest) = p.split_first_chunk::<4>()?;
    let (pos, pcm) = rest.split_first_chunk::<8>()?;
    let pos = u64::from_le_bytes(*pos);
    // Datagrams carry whole 240-frame blocks from position 0 (PROTOCOL.md), so `pos` is a multiple of 240.
    if pcm.len() != PACKET_FRAMES * 4 || !pos.is_multiple_of(PACKET_FRAMES_U64) {
        return None;
    }
    let pcm = pcm.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
    Some((u32::from_le_bytes(*sid), Packet { pos, t_us, pcm }))
}

/// The session id of an `AUDIO_START` this TV can play (48 kHz s16 stereo PCM, 240-frame datagrams).
///
/// # Errors
///
/// Not JSON, another format, or no `sid`.
pub(crate) fn parse_start(payload: &[u8]) -> Result<u32, String> {
    let v: serde_json::Value = serde_json::from_slice(payload).map_err(|e| e.to_string())?;
    let num = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let ok = v.get("codec").and_then(serde_json::Value::as_str) == Some("pcm_s16le")
        && num("rate") == Some(u64::from(RATE))
        && num("ch") == Some(2)
        && num("frames") == Some(PACKET_FRAMES_U64);
    if !ok {
        return Err(format!("unsupported audio format {v}"));
    }
    num("sid").and_then(|s| u32::try_from(s).ok()).ok_or_else(|| "AUDIO_START without sid".into())
}

/// The video delay audio follows: the median of the last `window` samples (TV µs - sender µs of the frames
/// shown), only once the window is full and after the first `warmup` samples of a video run are left out (an
/// encoder or decoder that just started is late for a moment; audio must not follow that).
#[derive(Debug)]
pub(crate) struct VideoTrack {
    samples: VecDeque<i64>,
    window: usize,
    warmup: usize,
    seen: usize,
}

impl VideoTrack {
    pub(crate) fn new(window: usize, warmup: usize) -> VideoTrack {
        VideoTrack { samples: VecDeque::with_capacity(window + 1), window, warmup, seen: 0 }
    }

    /// One sample of the shown video's delay.
    pub(crate) fn push(&mut self, delay_us: i64) {
        self.seen += 1;
        if self.seen <= self.warmup {
            return;
        }
        self.samples.push_back(delay_us);
        while self.samples.len() > self.window {
            self.samples.pop_front();
        }
    }

    /// The video stopped (or no fresh timing): start over.
    pub(crate) fn clear(&mut self) {
        self.samples.clear();
        self.seen = 0;
    }

    /// The median delay, once the window is full.
    pub(crate) fn value(&self) -> Option<i64> {
        if self.samples.len() < self.window {
            return None;
        }
        let mut s: Vec<i64> = self.samples.iter().copied().collect();
        s.sort_unstable();
        s.get(s.len() / 2).copied()
    }
}

/// Session totals (PROTOCOL.md STATS keys).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    /// Pulls that had audio missing because it had not arrived yet.
    pub(crate) underruns: u64,
    /// Datagrams dropped: arrived after their play time, or pushed out of the 1 s window.
    pub(crate) drops: u64,
    /// Datagrams missing at their play time while later audio was there.
    pub(crate) lost: u64,
    /// Frames skipped / silence frames inserted by resyncs.
    pub(crate) skip_frames: u64,
    pub(crate) insert_frames: u64,
    /// One-frame drift corrections. No longer made (small errors are resampled), so 0; the key stays for tools
    /// that read it.
    pub(crate) slips: u64,
    /// Frames gained or lost by rate adjustment (resampling), in total.
    pub(crate) adjust_frames: u64,
}

/// A STATS view of the playout.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Snapshot {
    /// Audio received and not yet played (ms).
    pub(crate) buf_ms: f32,
    /// Jitter target `J` (ms).
    pub(crate) target_ms: f32,
    /// `P - d_min` (ms): the whole wait the TV adds from the fastest arrival to presentation (jitter target,
    /// output latency, video sync).
    pub(crate) delay_ms: f32,
    /// The play delay follows the video.
    pub(crate) video_sync: bool,
    pub(crate) totals: Totals,
    /// (sender time, TV time) of the last frame handed out with real audio.
    pub(crate) play: Option<(u64, u64)>,
}

#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "stats: values far below 2^24")]
fn us_to_ms(us: i64) -> f32 {
    us as f32 / 1000.0
}

/// Frames -> µs at [`RATE`] (rounded toward zero).
fn frames_to_us(frames: i64) -> i64 {
    frames * 1_000_000 / i64::from(RATE)
}

/// µs -> frames at [`RATE`] (rounded to nearest).
fn us_to_frames(us: i64) -> i64 {
    (us * i64::from(RATE) + us.signum() * 500_000) / 1_000_000
}

/// Whole frames of a non-negative, small `f64` (a resampling step or a running total).
#[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "0 <= x < 2^32")]
fn whole_frames(x: f64) -> u64 {
    x.max(0.0) as u64
}

/// The stereo frame `t` of the way from `a` to `b` (`0 <= t < 1`).
#[expect(clippy::as_conversions, clippy::cast_possible_truncation, reason = "the result lies between two i16")]
fn lerp(a: [i16; 2], b: [i16; 2], t: f64) -> [i16; 2] {
    let mix = |x: i16, y: i16| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as i16;
    [mix(a[0], b[0]), mix(a[1], b[1])]
}

fn signed(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// Takes the datagrams a [`Playout`] is done with, so they are freed off the audio thread. It must not block or
/// allocate (it runs on the audio callback); a datagram it cannot take it drops.
pub(crate) struct Recycle(pub(crate) Box<dyn FnMut(Packet) + Send>);

impl std::fmt::Debug for Recycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Recycle")
    }
}

/// The jitter buffer and playout clock for one audio stream (one `sid`).
///
/// It runs on the `AAudio` data callback, so after [`Playout::default`] it does not allocate: datagrams live in a
/// fixed ring of slots, the delay window and the jitter scratch have their room reserved, and datagrams it is done
/// with go to its [`Recycle`] (when set) to be freed on another thread.
#[derive(Debug)]
pub(crate) struct Playout {
    /// Datagram slots, indexed by datagram number (`pos / 240`) modulo [`MAX_PACKETS`].
    slots: Vec<Option<Packet>>,
    /// Where finished datagrams go to be freed off this thread (dropped here when unset).
    recycle: Option<Recycle>,
    /// (arrival TV µs, arrival - `t_us`) samples of the last [`WINDOW_US`].
    window: VecDeque<(u64, i64)>,
    /// Reused buffer for the jitter percentile.
    scratch: Vec<i64>,
    target_us: i64,
    /// When the jitter last reached the target (TV µs; 0 = never): it does not shrink before [`TARGET_HOLD_US`]
    /// after this.
    target_raised_us: u64,
    /// Video delay + A/V offset (TV µs - sender µs) the audio follows, when the video is being shown.
    video_us: Option<i64>,
    /// Play delay `P` in force (None until the first datagram).
    delay_us: Option<i64>,
    d_min: i64,
    /// Output lead in force: how far ahead of its presentation a pull must have its audio (output latency
    /// plus one pull), and the largest lead seen since the last update.
    lead_us: i64,
    lead_max_us: i64,
    /// When the lead last reached `lead_us` (TV µs; 0 = never).
    lead_raised_us: u64,
    /// Fraction of a frame between `cursor` and the read position (resampling), `[0, 1)`.
    phase: f64,
    /// The rate is being adjusted (between [`ADJUST_START`] and [`ADJUST_STOP`]).
    adjusting: bool,
    /// Frames gained or lost by resampling, not yet counted in whole frames.
    adjust_acc: f64,
    video_sync: bool,
    next_update_us: u64,
    /// Next frame to hand out.
    cursor: Option<u64>,
    /// While starting: the lowest position received (frames before it are pre-roll, not losses).
    floor: Option<u64>,
    /// End of the newest datagram received.
    newest_end: u64,
    /// The newest (pos, `t_us`) seen: maps positions to sender time when the buffer is empty.
    anchor: Option<(u64, u64)>,
    last_arrival_us: u64,
    lost_frames: u64,
    /// A silence resync is in progress (runs until the position error is within the deadband).
    inserting: bool,
    totals: Totals,
    play: Option<(u64, u64)>,
}

impl Default for Playout {
    fn default() -> Self {
        Playout {
            slots: std::iter::repeat_with(|| None).take(MAX_PACKETS).collect(),
            recycle: None,
            window: VecDeque::with_capacity(WINDOW_CAP),
            scratch: Vec::with_capacity(WINDOW_CAP),
            target_us: TARGET_START_US,
            target_raised_us: 0,
            video_us: None,
            delay_us: None,
            d_min: 0,
            lead_us: 0,
            lead_max_us: 0,
            lead_raised_us: 0,
            phase: 0.0,
            adjusting: false,
            adjust_acc: 0.0,
            video_sync: false,
            next_update_us: 0,
            cursor: None,
            floor: None,
            newest_end: 0,
            anchor: None,
            last_arrival_us: 0,
            lost_frames: 0,
            inserting: false,
            totals: Totals::default(),
            play: None,
        }
    }
}

impl Playout {
    /// A playout that hands the datagrams it is done with to `recycle`.
    pub(crate) fn with_recycle(recycle: Recycle) -> Playout {
        Playout { recycle: Some(recycle), ..Playout::default() }
    }

    fn slot_of(pos: u64) -> usize {
        usize::try_from(pos / PACKET_FRAMES_U64 % u64::try_from(MAX_PACKETS).unwrap_or(1)).unwrap_or(0)
    }

    /// The datagram starting at `pos` (a multiple of 240), if held.
    fn packet(&self, pos: u64) -> Option<&Packet> {
        self.slots[Self::slot_of(pos)].as_ref().filter(|p| p.pos == pos)
    }

    /// Done with a datagram: free it off the audio thread when possible.
    fn retire(&mut self, p: Packet) {
        if let Some(r) = self.recycle.as_mut() {
            (r.0)(p);
        }
    }

    /// The held datagrams, newest first (at most [`MAX_PACKETS`] slots looked at).
    fn newest_first(&self) -> impl Iterator<Item = &Packet> {
        let newest = self.newest_end.checked_sub(PACKET_FRAMES_U64);
        (0..MAX_PACKETS)
            .map_while(move |k| {
                let back = u64::try_from(k).ok()? * PACKET_FRAMES_U64;
                newest?.checked_sub(back)
            })
            .filter_map(|pos| self.packet(pos))
    }

    /// The video's display delay plus the A/V offset (TV µs - sender µs), or None without video timing.
    pub(crate) fn set_video(&mut self, v: Option<i64>) {
        self.video_us = match (self.video_us, v) {
            (Some(old), Some(new)) if (new - old).abs() <= VIDEO_HYSTERESIS_US => Some(old),
            _ => v,
        };
    }

    /// A datagram arrived at TV time `arrive_us`.
    pub(crate) fn push(&mut self, pkt: Packet, arrive_us: u64) {
        if pkt.pcm.len() != PACKET_FRAMES * 2 || !pkt.pos.is_multiple_of(PACKET_FRAMES_U64) {
            return self.retire(pkt);
        }
        self.last_arrival_us = arrive_us;
        self.window.push_back((arrive_us, signed(arrive_us) - signed(pkt.t_us)));
        if self.delay_us.is_none() {
            self.update(arrive_us);
        }
        if self.cursor.is_some_and(|c| pkt.pos + PACKET_FRAMES_U64 <= c) {
            self.totals.drops += 1; // its time has passed
            return self.retire(pkt);
        }
        if self.cursor.is_none() {
            self.floor = Some(self.floor.map_or(pkt.pos, |f| f.min(pkt.pos)));
        }
        if self.anchor.is_none_or(|(p, _)| pkt.pos > p) {
            self.anchor = Some((pkt.pos, pkt.t_us));
        }
        let slot = Self::slot_of(pkt.pos);
        match self.slots[slot].as_ref().map(|p| p.pos) {
            Some(old) if old == pkt.pos => return self.retire(pkt), // a duplicate
            Some(old) if old > pkt.pos => {
                // Older than the 1 s the ring holds.
                self.totals.drops += 1;
                return self.retire(pkt);
            }
            Some(old) => {
                // Pushed out of the 1 s window by newer audio (counted only if it was still to be played).
                if self.cursor.is_none_or(|c| old + PACKET_FRAMES_U64 > c) {
                    self.totals.drops += 1;
                }
                if let Some(p) = self.slots[slot].take() {
                    self.retire(p);
                }
            }
            None => {}
        }
        self.newest_end = self.newest_end.max(pkt.pos + PACKET_FRAMES_U64);
        self.slots[slot] = Some(pkt);
    }

    /// Recompute the jitter target and the play delay (every [`UPDATE_US`]).
    fn update(&mut self, now: u64) {
        let newest = self.window.back().map_or(now, |w| w.0);
        while self.window.front().is_some_and(|w| w.0 + WINDOW_US < newest) {
            self.window.pop_front();
        }
        let Some(d_min) = self.window.iter().map(|w| w.1).min() else { return };
        // Delays beyond the largest target are Wi-Fi bursts, not jitter: no target could absorb them, so they
        // must not hold the latency up for the next seconds.
        let mut d = std::mem::take(&mut self.scratch);
        d.clear();
        d.extend(self.window.iter().map(|w| w.1 - d_min).filter(|&j| j <= TARGET_MAX_US));
        d.sort_unstable();
        let p95 = d[(d.len() - 1) * 95 / 100];
        self.scratch = d;
        let want = (p95 + TARGET_MARGIN_US).clamp(TARGET_MIN_US, TARGET_MAX_US);
        if want >= self.target_us {
            self.target_raised_us = now;
            self.target_us = want;
        } else if self.target_raised_us == 0 || now >= self.target_raised_us + TARGET_HOLD_US {
            self.target_us = want.max(self.target_us - TARGET_DECAY_US);
        }
        if self.lead_max_us > 0 {
            // Hold the largest output lead for 10 s, then let it come down slowly: the lead swings from burst to
            // burst, and following each swing moved the play delay and kept the playout correcting all the time.
            let l = std::mem::take(&mut self.lead_max_us);
            if l >= self.lead_us {
                self.lead_us = l;
                self.lead_raised_us = now;
            } else if now >= self.lead_raised_us + LEAD_HOLD_US {
                self.lead_us = l.max(self.lead_us - TARGET_DECAY_US);
            }
        }
        // The last frame a pull needs must have arrived when the pull runs.
        let base = d_min + self.target_us + self.lead_us;
        let p = self.video_us.map_or(base, |v| v.clamp(base, base + VIDEO_MAX_EXTRA_US));
        self.video_sync = self.video_us.is_some_and(|v| v > base);
        self.d_min = d_min;
        if self.delay_us.is_none_or(|old| (p - old).abs() > DELAY_HYSTERESIS_US) {
            self.delay_us = Some(p);
        }
        self.next_update_us = now + UPDATE_US;
    }

    /// The frame position whose sender time is `s` (µs), from the datagram nearest before it.
    fn position_at(&self, s: i64) -> Option<i64> {
        let mut oldest = None;
        let reference = self
            .newest_first()
            .find(|p| {
                oldest = Some((p.pos, p.t_us));
                signed(p.t_us) <= s
            })
            .map(|p| (p.pos, p.t_us))
            .or(oldest)
            .or(self.anchor)?;
        Some(signed(reference.0) + us_to_frames(s - signed(reference.1)))
    }

    /// Sender time of position `pos` (µs).
    fn time_of(&self, pos: u64) -> Option<u64> {
        let block = pos - pos % PACKET_FRAMES_U64;
        let held = self.newest_first().find(|p| p.pos <= block).map(|p| (p.pos, p.t_us));
        let (p, t) = held.or(self.anchor)?;
        u64::try_from(signed(t) + frames_to_us(signed(pos) - signed(p))).ok()
    }

    fn frame_at(&self, pos: u64) -> Option<[i16; 2]> {
        let k = pos % PACKET_FRAMES_U64;
        let p = self.packet(pos - k)?;
        let k = usize::try_from(k).ok()?;
        Some([p.pcm[2 * k], p.pcm[2 * k + 1]])
    }

    /// Free every held datagram that ends at or before `pos` (all of them for `u64::MAX`).
    fn retire_before(&mut self, pos: u64) {
        for i in 0..self.slots.len() {
            if self.slots[i].as_ref().is_some_and(|p| p.pos.saturating_add(PACKET_FRAMES_U64) <= pos) {
                if let Some(p) = self.slots[i].take() {
                    self.retire(p);
                }
            }
        }
    }

    /// Fill `out` (interleaved stereo) with the audio to present at TV time `present_us` (its first frame).
    /// `now_us` is the TV time of the call: `present_us - now_us` is the output latency.
    pub(crate) fn pull(&mut self, out: &mut [i16], present_us: u64, now_us: u64) {
        out.fill(0);
        let frames = out.len() / 2;
        let lead = signed(present_us) - signed(now_us) + frames_to_us(signed(u64::try_from(frames).unwrap_or(0)));
        self.lead_max_us = self.lead_max_us.max(lead);
        if present_us >= self.next_update_us || self.lead_us == 0 {
            self.update(present_us);
        }
        if self.cursor.is_some() && present_us.saturating_sub(self.last_arrival_us) > IDLE_RESET_US {
            // The stream stopped: forget its position, start over when audio returns.
            self.retire_before(u64::MAX);
            self.cursor = None;
            self.floor = None;
            self.anchor = None;
        }
        let Some(delay) = self.delay_us else { return };
        let Some(want) = self.position_at(signed(present_us) - delay) else { return };
        let mut start = 0usize; // output frames already decided (silence)
        let mut step = 1.0;
        let cursor = match self.cursor {
            None => match u64::try_from(want) {
                Ok(w) => {
                    self.phase = 0.0;
                    w
                }
                Err(_) => return,
            },
            Some(c) => {
                let err = want - signed(c);
                if err >= -SLIP_DEADBAND {
                    self.inserting = false;
                }
                if err > RESYNC_FRAMES {
                    self.totals.skip_frames += err.unsigned_abs();
                    self.phase = 0.0;
                    self.adjusting = false;
                    want.unsigned_abs()
                } else if err < -RESYNC_FRAMES || (self.inserting && err < -SLIP_DEADBAND) {
                    // Too early: silence until the right time comes (over several pulls, to the end).
                    self.inserting = true;
                    self.adjusting = false;
                    start = usize::try_from(err.unsigned_abs()).unwrap_or(frames).min(frames);
                    self.totals.insert_frames += u64::try_from(start).unwrap_or(0);
                    c
                } else {
                    // Small errors: play a little faster (behind) or slower (ahead) until the error is gone.
                    self.adjusting = err.abs() > ADJUST_START || (self.adjusting && err.abs() > ADJUST_STOP);
                    if self.adjusting {
                        let e = f64::from(i32::try_from(err).unwrap_or(0));
                        step += (e / (f64::from(RATE) * ADJUST_SECONDS)).clamp(-ADJUST_MAX, ADJUST_MAX);
                    }
                    c
                }
            }
        };
        let (mut pos, mut phase, mut waiting) = (cursor, self.phase, false);
        let mut first_real: Option<(u64, usize)> = None;
        for i in start..frames {
            if let Some(a) = self.frame_at(pos) {
                let lr = if phase > 0.0 { lerp(a, self.frame_at(pos + 1).unwrap_or(a), phase) } else { a };
                out[2 * i..2 * i + 2].copy_from_slice(&lr);
                first_real.get_or_insert((pos, i));
            } else if self.floor.is_some_and(|f| pos < f) {
                // pre-roll before the first datagram: silence, not a loss
            } else if pos < self.newest_end {
                self.lost_frames += 1;
            } else {
                waiting = true;
            }
            phase += step;
            let whole = phase.floor();
            phase -= whole;
            pos += whole_frames(whole);
        }
        self.phase = phase;
        if self.adjusting {
            self.adjust_acc += (step - 1.0).abs() * f64::from(u32::try_from(frames - start).unwrap_or(0));
            let whole = self.adjust_acc.floor();
            self.adjust_acc -= whole;
            self.totals.adjust_frames += whole_frames(whole);
        }
        if first_real.is_some() {
            self.floor = None;
        }
        self.cursor = Some(pos);
        self.totals.underruns += u64::from(waiting);
        self.totals.lost = self.lost_frames.div_ceil(PACKET_FRAMES_U64);
        self.retire_before(pos);
        if let Some((p, i)) = first_real {
            if let Some(t) = self.time_of(p) {
                let at = signed(present_us) + frames_to_us(signed(u64::try_from(i).unwrap_or(0)));
                self.play = u64::try_from(at).ok().map(|at| (t, at));
            }
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let ahead = self.cursor.map_or(0, |c| self.newest_end.saturating_sub(c));
        Snapshot {
            buf_ms: us_to_ms(frames_to_us(signed(ahead))),
            target_ms: us_to_ms(self.target_us),
            delay_ms: us_to_ms(self.delay_us.map_or(0, |p| p - self.d_min)),
            video_sync: self.video_sync,
            totals: self.totals,
            play: self.play,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TV clock = sender clock + this (unknown to the playout).
    const OFFSET: u64 = 7_000_000_000;
    /// One output burst: 192 frames = 4 ms, presented 10 ms after the pull (`AAudio` output latency).
    const BURST: usize = 192;
    const OUT_LATENCY_US: u64 = 10_000;
    /// `net` result for a datagram that never arrives.
    const LOST: u64 = u64::MAX;

    /// Frames carry their own position (`pos + 1` in 15-bit halves, so no real frame is silent).
    fn encode_pos(pos: u64) -> [i16; 2] {
        let v = pos + 1;
        [i16::try_from(v & 0x7FFF).unwrap(), i16::try_from((v >> 15) & 0x7FFF).unwrap()]
    }

    fn decode_pos(f: &[i16]) -> Option<u64> {
        let (l, r) = (u64::try_from(f[0]).ok()?, u64::try_from(f[1]).ok()?);
        (l | r != 0).then(|| (l | (r << 15)) - 1)
    }

    /// A sender + network + the TV's output clock, all in TV time.
    struct Sim {
        p: Playout,
        pos: u64,
        /// Sender time of position 0 (moved forward by a pause).
        t0: u64,
        /// Next pull, TV time.
        now: u64,
        pending: Vec<(u64, Packet)>,
        sent_t: std::collections::HashMap<u64, u64>,
        /// (present TV time, buffer) of every pull.
        out: Vec<(u64, Vec<i16>)>,
    }

    impl Sim {
        fn new() -> Sim {
            Sim {
                p: Playout::default(),
                pos: 0,
                t0: 1_000_000,
                now: 1_000_000 + OFFSET,
                pending: Vec::new(),
                sent_t: std::collections::HashMap::new(),
                out: Vec::new(),
            }
        }

        fn next_t(&self) -> u64 {
            self.t0 + self.pos * 1_000_000 / 48_000
        }

        /// `ms` of real time: the sender sends a datagram per 5 ms of audio (datagram `i` of this call is
        /// delayed `net(i)` µs, or lost), the TV pulls every 4 ms.
        fn run(&mut self, ms: u64, net: impl Fn(u64) -> u64) {
            let end = self.now + ms * 1000;
            let mut i = 0;
            while self.now < end {
                while self.next_t() + OFFSET <= self.now {
                    let t_us = self.next_t();
                    let pcm = (0..PACKET_FRAMES_U64).flat_map(|k| encode_pos(self.pos + k)).collect();
                    let d = net(i);
                    i += 1;
                    self.sent_t.insert(self.pos, t_us);
                    if d != LOST {
                        self.pending.push((t_us + OFFSET + d, Packet { pos: self.pos, t_us, pcm }));
                    }
                    self.pos += PACKET_FRAMES_U64;
                }
                self.pending.sort_by_key(|a| a.0);
                let n = self.pending.iter().take_while(|a| a.0 <= self.now).count();
                for (at, pkt) in self.pending.drain(..n) {
                    self.p.push(pkt, at);
                }
                let mut buf = vec![0i16; BURST * 2];
                let at = self.now + OUT_LATENCY_US;
                self.p.pull(&mut buf, at, self.now);
                self.out.push((at, buf));
                self.now += 4_000;
            }
        }

        /// The sender sends nothing for `ms` (nothing to capture); the TV keeps pulling.
        fn pause(&mut self, ms: u64) {
            self.t0 += ms * 1000;
            self.run(ms, |_| 3_000);
        }

        /// Capture -> presented latency of the last pull with real audio (µs).
        fn latency(&self) -> Option<i64> {
            self.out.iter().rev().find_map(|(at, buf)| {
                let i = buf.as_chunks::<2>().0.iter().position(|f| decode_pos(f).is_some())?;
                let pos = decode_pos(&buf[2 * i..])?;
                let base = pos - pos % PACKET_FRAMES_U64;
                let t = self.sent_t.get(&base)? + (pos - base) * 1_000_000 / 48_000;
                Some(signed(*at) + frames_to_us(signed(u64::try_from(i).unwrap())) - signed(t) - signed(OFFSET))
            })
        }

        /// Pulls (from the `from`-th on) that had any silent frame.
        fn gappy_pulls(&self, from: usize) -> usize {
            self.out[from..]
                .iter()
                .filter(|(_, b)| b.as_chunks::<2>().0.iter().any(|f| decode_pos(f).is_none()))
                .count()
        }
    }

    #[test]
    fn start_message_checks_the_format() {
        let ok = br#"{"v":1,"codec":"pcm_s16le","rate":48000,"ch":2,"frames":240,"sid":3735928559}"#;
        assert_eq!(parse_start(ok), Ok(3_735_928_559));
        assert!(parse_start(br#"{"v":1,"codec":"opus","rate":48000,"ch":2,"frames":240,"sid":1}"#).is_err());
        assert!(parse_start(br#"{"v":1,"codec":"pcm_s16le","rate":44100,"ch":2,"frames":240,"sid":1}"#).is_err());
        assert!(parse_start(br#"{"v":1,"codec":"pcm_s16le","rate":48000,"ch":2,"frames":240}"#).is_err());
        assert!(parse_start(b"nope").is_err());
    }

    #[test]
    fn parses_a_datagram_payload() {
        let mut p = 0xABu32.to_le_bytes().to_vec();
        p.extend_from_slice(&480u64.to_le_bytes());
        p.extend((0..480i16).flat_map(i16::to_le_bytes));
        let (sid, pkt) = parse_payload(99, &p).unwrap();
        assert_eq!((sid, pkt.pos, pkt.t_us, pkt.pcm.len()), (0xAB, 480, 99, 480));
        assert_eq!(pkt.pcm[3], 3);
        assert!(parse_payload(1, &p[..100]).is_none(), "short PCM");
        assert!(parse_payload(1, &[0; 6]).is_none());
    }

    #[test]
    fn steady_network_plays_at_net_delay_plus_the_target_with_no_glitches() {
        let mut s = Sim::new();
        s.run(3_000, |_| 3_000);
        // 3 ms network + the target (30 ms at the start, decaying to the 20 ms minimum on a clean link) + the
        // output lead (10 ms latency + one 4 ms pull).
        let l = s.latency().unwrap();
        assert!((36_000..=41_000).contains(&l), "latency {l} us");
        assert_eq!(s.gappy_pulls(20), 0, "no silence after the first datagrams");
        let snap = s.p.snapshot();
        let t = snap.totals;
        assert_eq!((t.underruns, t.drops, t.lost, t.skip_frames), (0, 0, 0, 0), "{snap:?}");
        assert!(snap.target_ms <= 21.0, "{snap:?}");
        assert!(snap.buf_ms > 5.0 && snap.buf_ms < 30.0, "{snap:?}");
        let (pt, at) = snap.play.unwrap();
        let reported = signed(at) - signed(pt) - signed(OFFSET);
        assert!((reported - l).abs() < 1_000, "STATS play pair {reported} vs measured {l}");
    }

    #[test]
    fn jitter_raises_the_target_until_late_datagrams_make_it() {
        let mut s = Sim::new();
        // One datagram in 7 is 35 ms late (more than the 5 % the target ignores).
        let net = |i: u64| if i.is_multiple_of(7) { 38_000 } else { 3_000 };
        s.run(3_000, net);
        let warm = s.p.snapshot();
        assert!(warm.target_ms >= 37.0, "{warm:?}");
        s.run(3_000, net);
        let snap = s.p.snapshot();
        assert_eq!(snap.totals.drops, warm.totals.drops, "after the target grew, late ones make it: {snap:?}");
        assert_eq!(snap.totals.underruns, warm.totals.underruns, "{snap:?}");
    }

    #[test]
    fn the_target_holds_its_peak_for_ten_seconds_then_shrinks() {
        let mut s = Sim::new();
        // 3 s with one datagram in 7 late by 35 ms: the target rises to cover it.
        s.run(3_000, |i: u64| if i.is_multiple_of(7) { 38_000 } else { 3_000 });
        let peak = s.p.snapshot().target_ms;
        assert!(peak >= 37.0, "{peak}");
        // A quiet link (a still screen): the target stays up for 10 s ...
        s.run(9_000, |_| 3_000);
        let held = s.p.snapshot().target_ms;
        assert!((held - peak).abs() < 0.01, "held: {peak} -> {held}");
        // ... then shrinks back to the floor.
        s.run(6_000, |_| 3_000);
        let later = s.p.snapshot().target_ms;
        assert!(later <= 21.0, "shrank after the hold: {later}");
        // Late audio after the quiet stretch, within the held target, is not dropped.
        let mut s2 = Sim::new();
        s2.run(3_000, |i: u64| if i.is_multiple_of(7) { 38_000 } else { 3_000 });
        s2.run(8_000, |_| 3_000);
        let before = s2.p.snapshot().totals;
        s2.run(1_000, |i: u64| if i.is_multiple_of(3) { 30_000 } else { 3_000 });
        let after = s2.p.snapshot().totals;
        assert_eq!((after.drops, after.underruns), (before.drops, before.underruns), "{after:?}");
    }

    #[test]
    fn a_wifi_burst_drops_late_audio_and_the_buffer_does_not_grow() {
        let mut s = Sim::new();
        s.run(2_000, |_| 3_000);
        let before = s.p.snapshot();
        // A 150 ms gap: the datagrams sent during it all arrive together at its end.
        s.run(1_000, |i| if i < 30 { 153_000 - i * 5_000 } else { 3_000 });
        let snap = s.p.snapshot();
        assert!(snap.totals.underruns > before.totals.underruns, "the gap plays as silence: {snap:?}");
        assert!(snap.totals.drops >= 20, "audio that came after its time was dropped: {snap:?}");
        assert!(snap.buf_ms <= before.buf_ms + 5.0, "no backlog after the burst: {before:?} -> {snap:?}");
        assert!(snap.target_ms < 30.0, "a burst is not jitter: the target stays low: {snap:?}");
        let l = s.latency().unwrap();
        assert!(l < 42_000, "back at low latency right after the burst: {l}");
        assert_eq!(s.gappy_pulls(s.out.len() - 100), 0, "clean audio after it");
    }

    #[test]
    fn clock_drift_is_taken_out_by_resampling_not_slips_or_resyncs() {
        let mut s = Sim::new();
        // The network delay creeps up 2 ms per second (strong drift between the clocks).
        s.run(5_000, |i| 3_000 + i * 10);
        let snap = s.p.snapshot();
        assert!(snap.totals.adjust_frames > 0, "{snap:?}");
        assert_eq!(snap.totals.slips, 0, "{snap:?}");
        assert_eq!((snap.totals.skip_frames, snap.totals.insert_frames), (0, 0), "{snap:?}");
        // d_min lags the rising delay by its 2 s window (~9 ms at the end) + 20 ms target + 14 ms lead.
        let l = s.latency().unwrap();
        assert!((38_000..=50_000).contains(&l), "follows the drift: {l}");
    }

    #[test]
    fn rate_adjustment_keeps_a_tone_smooth() {
        // A 440 Hz tone through a drifting network: the playout must adjust its rate, and the waveform may never
        // jump by more than the tone's own largest step (a dropped or repeated frame jumps by up to twice that).
        let amp = 8_000.0;
        let w = 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "test tone"
        )]
        let tone = |pos: u64| (amp * (w * pos as f64).sin()).round() as i16;
        let mut p = Playout::default();
        let (mut pos, mut now, mut sent) = (0u64, OFFSET, Vec::new());
        let mut out: Vec<i16> = Vec::new();
        for k in 0..1_500u64 {
            // Sender: datagrams up to `now`, delayed 3 ms + 10 us per datagram (2 ms/s of drift).
            while pos * 1_000_000 / 48_000 + OFFSET <= now {
                let t_us = pos * 1_000_000 / 48_000;
                let pcm = (0..PACKET_FRAMES_U64).flat_map(|f| [tone(pos + f), tone(pos + f)]).collect();
                sent.push((t_us + OFFSET + 3_000 + (pos / PACKET_FRAMES_U64) * 10, Packet { pos, t_us, pcm }));
                pos += PACKET_FRAMES_U64;
            }
            sent.sort_by_key(|a| a.0);
            let n = sent.iter().take_while(|a| a.0 <= now).count();
            for (at, pkt) in sent.drain(..n) {
                p.push(pkt, at);
            }
            let mut buf = vec![0i16; BURST * 2];
            p.pull(&mut buf, now + OUT_LATENCY_US, now);
            if k >= 250 {
                out.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
            }
            now += 4_000;
        }
        let t = p.snapshot().totals;
        assert!(t.adjust_frames > 50, "the drift needs rate adjustment: {t:?}");
        assert_eq!((t.slips, t.skip_frames, t.insert_frames, t.underruns), (0, 0, 0, 0), "{t:?}");
        // Playing 0.5 % fast makes the tone itself 0.5 % steeper; +2 for rounding.
        let max_step = amp * w * (1.0 + ADJUST_MAX) + 2.0;
        let worst = out.windows(2).map(|x| (i32::from(x[1]) - i32::from(x[0])).abs()).max().unwrap();
        assert!(f64::from(worst) <= max_step, "a jump of {worst} (the tone moves at most {max_step:.0} per frame)");
    }

    #[test]
    fn the_output_lead_holds_its_peak_for_ten_seconds() {
        let mut p = Playout::default();
        let mut buf = vec![0i16; BURST * 2];
        let mut now = 1_000_000u64;
        // A datagram every 5 ms (the delay estimate needs them) and one pull each, presented `latency` later.
        let mut step = |p: &mut Playout, now: u64, latency: u64| {
            let pos = now / 5_000 * PACKET_FRAMES_U64;
            p.push(Packet { pos, t_us: now - 3_000, pcm: vec![0; PACKET_FRAMES * 2] }, now);
            p.pull(&mut buf, now + latency, now);
        };
        step(&mut p, now, 40_000);
        let peak = p.lead_us;
        while now < 10_000_000 {
            now += 5_000;
            step(&mut p, now, 10_000);
        }
        assert_eq!(p.lead_us, peak, "held for 10 s");
        while now < 13_000_000 {
            now += 5_000;
            step(&mut p, now, 10_000);
        }
        assert!(p.lead_us < peak, "shrinks after the hold: {} -> {}", peak, p.lead_us);
    }

    #[test]
    fn finished_datagrams_go_to_the_recycle_channel() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1_000);
        let mut s = Sim::new();
        s.p = Playout::with_recycle(Recycle(Box::new(move |p| {
            let _ = tx.try_send(p);
        })));
        // A steady second, then a duplicate of the newest datagram and one too old for the ring.
        s.run(1_000, |_| 3_000);
        let newest = s.pos - PACKET_FRAMES_U64;
        let pcm = vec![0; PACKET_FRAMES * 2];
        s.p.push(Packet { pos: newest, t_us: 0, pcm: pcm.clone() }, s.now);
        s.p.push(Packet { pos: 0, t_us: 0, pcm }, s.now);
        let held = s.p.slots.iter().flatten().count();
        let recycled = rx.try_iter().count();
        // Every datagram sent is either still held or was handed back (the last few may still be in flight).
        let sent = usize::try_from(s.pos / PACKET_FRAMES_U64).unwrap() + 2;
        assert!(recycled + held + s.pending.len() == sent, "sent {sent}, held {held}, recycled {recycled}");
        assert!(recycled > 150, "{recycled}");
    }

    #[test]
    fn video_sync_delays_audio_to_the_picture_within_limits() {
        let mut s = Sim::new();
        // The picture is shown 90 ms after capture: audio follows it.
        s.p.set_video(Some(signed(OFFSET) + 90_000));
        s.run(2_000, |_| 3_000);
        let l = s.latency().unwrap();
        assert!((87_000..=93_000).contains(&l), "audio at the picture's delay: {l}");
        assert!(s.p.snapshot().video_sync);
        // The picture faster than the audio path: audio cannot go below its own minimum.
        s.p.set_video(Some(signed(OFFSET)));
        s.run(2_000, |_| 3_000);
        let l = s.latency().unwrap();
        assert!(l < 42_000, "{l}");
        assert!(!s.p.snapshot().video_sync);
        // A 2 s video backlog: audio follows by at most 300 ms more.
        s.p.set_video(Some(signed(OFFSET) + 2_000_000));
        s.run(2_000, |_| 3_000);
        let l = s.latency().unwrap();
        assert!((330_000..=345_000).contains(&l), "{l}");
    }

    #[test]
    fn video_track_skips_the_warmup_and_needs_a_full_window() {
        let mut v = VideoTrack::new(4, 2);
        for x in [600_000, 500_000] {
            v.push(x); // a starting encoder: late
        }
        assert_eq!(v.value(), None);
        for x in [20_000, 22_000, 90_000] {
            v.push(x);
        }
        assert_eq!(v.value(), None, "window not full");
        v.push(21_000);
        assert_eq!(v.value(), Some(22_000), "median of the last 4, warm-up left out");
        v.clear();
        v.push(1);
        assert_eq!(v.value(), None);
    }

    #[test]
    fn small_video_jitter_does_not_move_the_audio() {
        let mut s = Sim::new();
        s.p.set_video(Some(signed(OFFSET) + 90_000));
        s.run(2_000, |_| 3_000);
        let before = s.p.snapshot().totals.slips;
        // The measured video delay wobbles by +-6 ms every 100 ms.
        for k in 0..30 {
            let wobble = if k % 2 == 0 { 6_000 } else { -6_000 };
            s.p.set_video(Some(signed(OFFSET) + 90_000 + wobble));
            s.run(100, |_| 3_000);
        }
        assert_eq!(s.p.snapshot().totals.slips, before, "no slips for video jitter");
        let l = s.latency().unwrap();
        assert!((87_000..=93_000).contains(&l), "{l}");
    }

    #[test]
    fn lost_datagrams_are_counted_and_play_as_silence() {
        let mut s = Sim::new();
        s.run(1_000, |_| 3_000);
        // Every 10th datagram of the next second never arrives.
        s.run(1_000, |i| if i % 10 == 3 { LOST } else { 3_000 });
        s.run(200, |_| 3_000);
        let snap = s.p.snapshot();
        assert!((19..=21).contains(&snap.totals.lost), "{snap:?}");
        assert_eq!(snap.totals.underruns, 0, "a loss is not an underrun: {snap:?}");
    }

    #[test]
    fn a_stopped_stream_starts_over_cleanly() {
        let mut s = Sim::new();
        s.run(1_000, |_| 3_000);
        s.pause(2_000);
        s.run(1_000, |_| 3_000);
        let l = s.latency().unwrap();
        assert!(l < 42_000, "{l}");
        let t = s.p.snapshot().totals;
        assert_eq!((t.skip_frames, t.drops), (0, 0), "a fresh start, not a 2 s skip: {t:?}");
    }
}
