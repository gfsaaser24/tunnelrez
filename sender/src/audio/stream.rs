//! The audio worker of one TV session: waits for the TV's CAPS, sends `AUDIO_START` over TCP, then captures
//! the PC's playback (WASAPI loopback) and sends 5 ms AUDIO datagrams over UDP until the session ends.
//!
//! With a virtual sink (see [`super::sink`]) the sink is the default playback device and loopback captures it;
//! the PC speakers are silent unless "Also play on PC" (`--audio on`) echoes the sound to them. Without one,
//! loopback captures the PC speakers themselves, and "Play on TV only" mutes them.

use super::pcm::InFormat;
use super::pcm::{frames_to_us, lead_t, Frame, Packet, Packetizer, Resampler, PACKET_FRAMES, RATE};
use super::sink::{self, Route, SinkChoice};
use super::tv_only::{self, TvOnly};
use super::wasapi::{Chunk, Com, Loopback, Render};
use crate::clock;
use crate::proto::{build, T_AUDIO_START, T_AUDIO_STOP};
use anyhow::{Context, Result};
use serde_json::json;
use std::hash::{BuildHasher, RandomState};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `--audio`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AudioMode {
    Off,
    /// Audio to the TV; the PC keeps playing too ("Also play on PC": with a virtual sink, the sound is echoed
    /// to the PC speakers).
    #[default]
    On,
    /// Audio to the TV only: with a virtual sink the PC speakers are just not the default; without one they are
    /// muted while streaming.
    TvOnly,
}

impl AudioMode {
    /// Parse `on` / `off` / `tv-only`.
    ///
    /// # Errors
    ///
    /// Any other value.
    pub fn parse(s: &str) -> Result<AudioMode, String> {
        match s {
            "on" => Ok(AudioMode::On),
            "off" => Ok(AudioMode::Off),
            "tv-only" => Ok(AudioMode::TvOnly),
            _ => Err(format!("--audio {s}: use on, off or tv-only")),
        }
    }
}

/// How long to wait for the TV's CAPS before treating it as a TV without audio.
const CAPS_WAIT: Duration = Duration::from_secs(5);
/// No loopback data for this long = nothing plays: send silence datagrams instead.
const SILENCE_AFTER_US: u64 = 20_000;
/// Silence datagrams stay this far behind "now", so real data that resumes does not overlap them.
const SILENCE_LAG_US: u64 = 15_000;
/// Loopback wait per read: half a datagram.
const READ_WAIT_MS: u32 = 3;

/// What the session knows about audio, shared by the reader thread (CAPS), the audio worker and the stats line.
#[derive(Debug, Default)]
pub struct Link {
    /// 0 = no CAPS yet, 1 = the TV plays audio, 2 = it does not.
    tv_audio: AtomicU8,
    sent: AtomicU64,
    cap_gaps: AtomicU64,
    state: Mutex<&'static str>,
    tv_only: AtomicBool,
    /// Friendly name of the captured device.
    device: Mutex<String>,
    /// 0 = not capturing, 1 = a virtual sink, 2 = plain loopback of the PC speakers.
    route: AtomicU8,
    /// "Also play on PC" is echoing to the PC speakers.
    pc_echo: AtomicBool,
}

/// A snapshot for the 1 Hz stats line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStats {
    pub state: &'static str,
    pub tv_only: bool,
    /// Datagrams sent since the last snapshot.
    pub sent: u64,
    pub cap_gaps: u64,
    /// Friendly name of the captured device ("" before capture starts).
    pub device: String,
    /// `sink`, `loopback`, or "" before capture starts.
    pub route: &'static str,
    pub pc_echo: bool,
}

impl Link {
    /// The TV's CAPS arrived: does its `caps` list `audio`?
    pub fn on_caps(&self, caps: &serde_json::Value) {
        let yes =
            caps.get("caps").and_then(serde_json::Value::as_array).is_some_and(|a| a.iter().any(|c| c == "audio"));
        self.tv_audio.store(if yes { 1 } else { 2 }, SeqCst);
    }

    /// The worker could not start (no TV address): the stats line says `error`.
    pub fn set_error(&self) {
        self.set_state("error");
    }

    fn set_state(&self, s: &'static str) {
        *self.state.lock().expect("audio state lock poisoned") = s;
    }

    /// State and counters; resets the per-second datagram count.
    ///
    /// # Panics
    ///
    /// When the state lock is poisoned (the audio thread panicked).
    pub fn take_stats(&self) -> LinkStats {
        LinkStats {
            state: *self.state.lock().expect("audio state lock poisoned"),
            tv_only: self.tv_only.load(SeqCst),
            sent: self.sent.swap(0, SeqCst),
            cap_gaps: self.cap_gaps.load(SeqCst),
            device: self.device.lock().expect("audio device lock poisoned").clone(),
            route: match self.route.load(SeqCst) {
                1 => "sink",
                2 => "loopback",
                _ => "",
            },
            pc_echo: self.pc_echo.load(SeqCst),
        }
    }
}

/// What the worker needs from its session.
pub struct Session<'a> {
    pub link: &'a Link,
    /// The TV's TCP address (datagrams go to the same IP and port).
    pub tv: SocketAddr,
    /// Send one message on the session's TCP stream; false = the connection is gone.
    pub send_tcp: &'a dyn Fn(&[u8]) -> bool,
    /// True once the session is over.
    pub done: &'a dyn Fn() -> bool,
    /// `--audio-sink`.
    pub sink: &'a SinkChoice,
}

impl std::fmt::Debug for Session<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("tv", &self.tv).finish_non_exhaustive()
    }
}

/// Run audio for one session (call on its own thread). Returns when the session ends.
pub fn run(mode: AudioMode, s: &Session<'_>) {
    s.link.set_state(if mode == AudioMode::Off { "off" } else { "waiting" });
    if mode == AudioMode::Off || !wait_for_caps(s) {
        return;
    }
    if let Err(e) = stream(mode, s) {
        s.link.set_state("error");
        eprintln!("[audio] stopped: {e:#}");
    }
}

/// Wait for CAPS. True when the TV plays audio.
fn wait_for_caps(s: &Session<'_>) -> bool {
    let t0 = Instant::now();
    let mut late = false;
    loop {
        match s.link.tv_audio.load(SeqCst) {
            1 => {
                if late {
                    eprintln!("[audio] the TV's CAPS came late; starting audio now");
                }
                return true;
            }
            2 => break,
            _ if (s.done)() => return false,
            // No CAPS yet: report "no audio" but keep listening, so a late CAPS still starts audio.
            _ if !late && t0.elapsed() >= CAPS_WAIT => {
                late = true;
                s.link.set_state("tv_no_audio");
                eprintln!("[audio] no CAPS from the TV after {} s; video only until it arrives", CAPS_WAIT.as_secs());
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    if !(s.done)() {
        s.link.set_state("tv_no_audio");
        eprintln!("[audio] the TV app has no audio support (update it); streaming video only");
    }
    false
}

/// A random session id for `AUDIO_START`.
fn new_sid() -> u32 {
    let h = RandomState::new().hash_one(clock::now_us());
    u32::try_from(h & 0xFFFF_FFFF).unwrap_or(0)
}

/// `AUDIO_START` payload.
fn start_json(sid: u32) -> String {
    json!({
        "v": 1, "codec": "pcm_s16le", "rate": RATE, "ch": 2,
        "frames": PACKET_FRAMES, "sid": sid,
    })
    .to_string()
}

/// Ask Windows to schedule this thread as audio (MMCSS "Pro Audio"). Best effort.
fn raise_priority() {
    use windows::core::w;
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut task = 0u32;
    // SAFETY: `task` is a valid out-parameter; the name is a static NUL-terminated wide string.
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &raw mut task) };
}

fn signed(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// The capture -> datagram loop for one session.
fn stream(mode: AudioMode, s: &Session<'_>) -> Result<()> {
    let _com = Com::init()?;
    raise_priority();
    let udp = UdpSocket::bind(("0.0.0.0", 0)).context("UDP socket")?;
    udp.connect(s.tv).with_context(|| format!("UDP to {}", s.tv))?;
    // Voice priority on the wire (WMM AC_VO on Wi-Fi), so audio does not queue behind video frames. Declared
    // after `udp`, so it is dropped first.
    let _voice = match super::qos::VoiceFlow::new(&udp) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("[audio] no voice priority for the audio datagrams: {e}");
            None
        }
    };
    // The virtual sink first: it becomes the default, so the loopback below opens on it.
    let route = match sink::acquire(s.sink) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[audio] virtual sink unavailable ({e:#}); using plain loopback");
            None
        }
    };
    if route.is_none() && *s.sink == SinkChoice::Auto {
        eprintln!(
            "[audio] no virtual audio device found; capturing the PC speakers. For best results install the TV \
             speaker: tunnelrez-send audio-sink install (the app's Install TV speaker button)"
        );
    }
    let mut cap = open_loopback(s)?;
    let mut out = Outputs::default();
    let sid = new_sid();
    let mut tx = Sender::new(&udp, sid, s.link, cap.format.clone());
    out.apply(mode, route.as_ref(), &cap, &mut tx, s)?;
    if !(s.send_tcp)(&build(T_AUDIO_START, 0, clock::now_us(), &[start_json(sid).as_bytes()])) {
        return Ok(());
    }
    eprintln!(
        "[audio] on: loopback of {} ({} Hz {} ch) -> {RATE} Hz s16 stereo, 5 ms UDP datagrams to {}{}",
        cap.name,
        cap.format.rate,
        cap.format.channels(),
        s.tv,
        out.describe(mode)
    );
    s.link.set_state("on");
    let mut last_check = Instant::now();
    while !(s.done)() {
        let r = cap.read(READ_WAIT_MS, |c| tx.chunk(&c, clock::now_us()));
        if let Err(e) = r {
            // The device may be gone: Windows then makes another one the default, and that is what reopens.
            eprintln!("[audio] capture error ({e:#}); reopening the playback device");
            std::thread::sleep(Duration::from_millis(200));
            reopen(mode, route.as_ref(), s, &mut cap, &mut tx, &mut out)?;
        } else if last_check.elapsed() >= Duration::from_secs(1) {
            last_check = Instant::now();
            if cap.default_changed() {
                eprintln!("[audio] default playback device changed; capturing the new one");
                reopen(mode, route.as_ref(), s, &mut cap, &mut tx, &mut out)?;
            }
        }
        out.keep_alive();
        tx.fill_silence(clock::now_us());
    }
    // The TV stops first; the PC speakers come back after (a mute or the sink switch is kept a few seconds,
    // in case the next session starts right away).
    (s.send_tcp)(&build(T_AUDIO_STOP, 0, clock::now_us(), &[]));
    tx.echo = None;
    out.keepalive = None;
    if let Some(t) = out.tv_only.take() {
        tv_only::park(t);
    }
    drop(route);
    s.link.tv_only.store(false, SeqCst);
    s.link.pc_echo.store(false, SeqCst);
    eprintln!("[audio] off");
    Ok(())
}

/// Capture the default playback device again (after a capture error or a default-device change); the mute and
/// the echo follow the captured device.
fn reopen(
    mode: AudioMode,
    route: Option<&Route>,
    s: &Session<'_>,
    cap: &mut Loopback,
    tx: &mut Sender<'_>,
    out: &mut Outputs,
) -> Result<()> {
    *cap = open_loopback(s)?;
    tx.format_changed(cap.format.clone());
    out.apply(mode, route, cap, tx, s)
}

/// What the PC speakers do while audio streams: muted ("Play on TV only" without a sink), or echoing the sink
/// ("Also play on PC" with one), or nothing.
#[derive(Debug, Default)]
struct Outputs {
    tv_only: Option<TvOnly>,
    /// Digital silence played to the captured device for the whole stream. Windows delivers loopback packets
    /// only while something plays on the device; with this stream it delivers them all the time, so the
    /// datagram stamps stay one continuous line of WASAPI times (no jump from the sender's own silence stamps
    /// to the first real packet when sound starts) and the device's audio engine never idles. The sender's
    /// silence datagrams (`fill_silence`) remain the fallback when this stream cannot run.
    keepalive: Option<Render>,
}

/// What the PC speakers should do for the captured device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Mute the captured device (it is the PC speakers, and the user wants TV only).
    Mute,
    /// Echo the sink to the PC device that was the default before.
    Echo,
    Nothing,
}

/// The plan for `mode` when capturing `captured`, with the sink `sink` (if any) and the old PC default `pc`.
fn plan(mode: AudioMode, sink: Option<&str>, pc: Option<&str>, captured: &str) -> Plan {
    let on_sink = sink == Some(captured);
    match mode {
        AudioMode::TvOnly if !on_sink => Plan::Mute,
        AudioMode::On if on_sink && pc.is_some_and(|p| p != captured) => Plan::Echo,
        _ => Plan::Nothing,
    }
}

impl Outputs {
    fn apply(
        &mut self,
        mode: AudioMode,
        route: Option<&Route>,
        cap: &Loopback,
        tx: &mut Sender<'_>,
        s: &Session<'_>,
    ) -> Result<()> {
        let sink = route.map(|r| r.sink.id.as_str());
        let pc = route.and_then(|r| r.pc_device.as_deref());
        let p = plan(mode, sink, pc, &cap.device);
        if p == Plan::Mute {
            if self.tv_only.is_none() || mute_moves(self.tv_only.as_ref().map(TvOnly::device), &cap.device) {
                drop(self.tv_only.take()); // unmutes the old device first
                self.tv_only = Some(engage_tv_only(s)?);
            }
        } else {
            drop(self.tv_only.take());
        }
        match (p, pc) {
            (Plan::Echo, Some(pc)) if tx.echo.as_ref().is_none_or(|e| e.device != pc) => {
                tx.echo = match Render::open(pc) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        eprintln!("[audio] Also play on PC: cannot play to the PC speakers ({e:#})");
                        None
                    }
                };
            }
            (Plan::Echo, _) => {}
            _ => tx.echo = None,
        }
        if self.keepalive.as_ref().is_none_or(|k| k.device != cap.device) {
            self.keepalive = match Render::open(&cap.device) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("[audio] no silence keep-alive on the captured device ({e:#}); sending silence instead");
                    None
                }
            };
        }
        s.link.device.lock().expect("audio device lock poisoned").clone_from(&cap.name);
        s.link.route.store(if sink == Some(cap.device.as_str()) { 1 } else { 2 }, SeqCst);
        s.link.tv_only.store(mode == AudioMode::TvOnly, SeqCst);
        s.link.pc_echo.store(tx.echo.is_some(), SeqCst);
        Ok(())
    }

    /// Top up the silence keep-alive; on an error it stops (the silence datagrams take over).
    fn keep_alive(&mut self) {
        if let Some(k) = self.keepalive.as_mut() {
            if let Err(e) = k.fill_silence() {
                eprintln!("[audio] silence keep-alive stopped ({e:#}); sending silence instead");
                self.keepalive = None;
            }
        }
    }

    fn describe(&self, mode: AudioMode) -> &'static str {
        match (mode, self.tv_only.is_some()) {
            (AudioMode::TvOnly, true) => ", PC speakers muted (Play on TV only)",
            (AudioMode::TvOnly, false) => ", TV only",
            _ => "",
        }
    }
}

/// TV-only mode muted another endpoint than the one now captured: the mute must move.
fn mute_moves(muted: Option<&str>, captured: &str) -> bool {
    muted.is_some_and(|m| m != captured)
}

/// Open loopback capture, retrying once a second until it works or the session ends.
fn open_loopback(s: &Session<'_>) -> Result<Loopback> {
    let mut logged = false;
    loop {
        match Loopback::open() {
            Ok(c) => return Ok(c),
            Err(e) if (s.done)() => return Err(e),
            Err(e) => {
                if !logged {
                    eprintln!("[audio] cannot capture the PC's playback yet ({e:#}); retrying");
                    s.link.set_state("error");
                    logged = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn engage_tv_only(s: &Session<'_>) -> Result<TvOnly> {
    let t = TvOnly::engage().context("Play on TV only: mute the PC speakers")?;
    if t.was_muted() {
        eprintln!("[audio] PC speakers already muted ({}); left as they are", t.device());
    }
    s.link.tv_only.store(true, SeqCst);
    Ok(t)
}

/// Loopback chunks -> 48 kHz stereo -> datagrams, plus silence while nothing plays.
struct Sender<'a> {
    udp: &'a UdpSocket,
    sid: u32,
    link: &'a Link,
    format: InFormat,
    pk: Packetizer,
    resampler: Option<Resampler>,
    frames: Vec<Frame>,
    out48: Vec<Frame>,
    packets: Vec<Packet>,
    /// Sender time just after the last frame packetized (where the next one belongs).
    next_t: u64,
    /// How far WASAPI's loopback stamps run ahead of the clock at read time (µs). Loopback stamps a frame
    /// with the time the PC's own output plays it (~30 ms after it can be read), which is also when the PC
    /// shows the matching picture. Silence uses the same time base, so the TV's delay estimate does not jump
    /// when sound starts or stops. None until the first real packet.
    ahead_us: Option<i64>,
    /// "Also play on PC": the converted audio also plays here.
    echo: Option<Render>,
}

impl<'a> Sender<'a> {
    fn new(udp: &'a UdpSocket, sid: u32, link: &'a Link, format: InFormat) -> Sender<'a> {
        let mut s = Sender {
            udp,
            sid,
            link,
            format: format.clone(),
            pk: Packetizer::default(),
            resampler: None,
            frames: Vec::new(),
            out48: Vec::new(),
            packets: Vec::new(),
            next_t: clock::now_us(),
            ahead_us: None,
            echo: None,
        };
        s.format_changed(format);
        s
    }

    fn format_changed(&mut self, format: InFormat) {
        self.resampler = (format.rate != RATE).then(|| Resampler::new(format.rate));
        self.format = format;
    }

    /// One loopback packet, read at sender time `now`.
    fn chunk(&mut self, c: &Chunk<'_>, now: u64) {
        if c.discontinuity {
            self.link.cap_gaps.fetch_add(1, SeqCst);
        }
        if let (Some(t), Some(_)) = (c.t_us, c.data) {
            self.ahead_us = Some(signed(t) - signed(now));
        }
        let t = c.t_us.unwrap_or(self.next_t);
        self.frames.clear();
        match c.data {
            Some(d) => self.format.decode(d, &mut self.frames),
            None => self.frames.resize(c.frames, [0.0, 0.0]),
        }
        self.out48.clear();
        let t48 = if let Some(r) = self.resampler.as_mut() {
            lead_t(t, r.process(&self.frames, &mut self.out48), self.format.rate)
        } else {
            self.out48.extend_from_slice(&self.frames);
            t
        };
        if let Some(e) = self.echo.as_mut() {
            if let Err(err) = e.push(&self.out48) {
                eprintln!("[audio] Also play on PC stopped: {err:#}");
                self.echo = None;
                self.link.pc_echo.store(false, SeqCst);
            }
        }
        self.pk.push(&self.out48, t48, &mut self.packets);
        self.next_t = t48 + frames_to_us(u64::try_from(self.out48.len()).unwrap_or(0));
        self.flush();
    }

    /// Nothing played for a while (Windows sends no loopback packets then): send silence up to shortly
    /// before `now` (in the loopback's time base), in whole datagrams. Nothing before the first real packet:
    /// its time base is not known yet.
    fn fill_silence(&mut self, now: u64) {
        let Some(ahead) = self.ahead_us else { return };
        let now = u64::try_from(signed(now) + ahead).unwrap_or(0);
        if now < self.next_t + SILENCE_AFTER_US {
            return;
        }
        let gap = now - SILENCE_LAG_US - self.next_t;
        let per = frames_to_us(u64::try_from(PACKET_FRAMES).unwrap_or(0));
        let n = usize::try_from(gap / per).unwrap_or(0) * PACKET_FRAMES;
        if n == 0 {
            return;
        }
        self.pk.push_silence(n, self.next_t, &mut self.packets);
        self.next_t += frames_to_us(u64::try_from(n).unwrap_or(0));
        self.flush();
    }

    fn flush(&mut self) {
        for p in self.packets.drain(..) {
            // A refused datagram (ICMP port unreachable from a TV app that is not listening) is not an error:
            // the TCP session decides when the TV is gone.
            if self.udp.send(&p.encode(self.sid)).is_ok() {
                self.link.sent.fetch_add(1, SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::pcm::Sample;
    use super::*;

    #[test]
    fn the_mute_follows_the_captured_device() {
        assert!(mute_moves(Some("{A}"), "{B}"), "capture reopened on B after A went away: mute B");
        assert!(!mute_moves(Some("{A}"), "{A}"), "same device: keep the mute");
        assert!(!mute_moves(None, "{B}"), "not TV-only: nothing to mute");
    }

    #[test]
    fn the_pc_speakers_plan() {
        let (sink, pc) = (Some("{cable}"), Some("{spk}"));
        assert_eq!(plan(AudioMode::TvOnly, sink, pc, "{cable}"), Plan::Nothing, "sink: TV only needs no mute");
        assert_eq!(plan(AudioMode::On, sink, pc, "{cable}"), Plan::Echo, "sink: also play on PC");
        assert_eq!(plan(AudioMode::On, sink, None, "{cable}"), Plan::Nothing, "sink was already default");
        assert_eq!(plan(AudioMode::TvOnly, None, None, "{spk}"), Plan::Mute, "no sink: mute");
        assert_eq!(plan(AudioMode::On, None, None, "{spk}"), Plan::Nothing, "no sink: PC plays anyway");
        // The user picked the speakers as default mid-stream: capture follows, like no sink.
        assert_eq!(plan(AudioMode::On, sink, pc, "{spk}"), Plan::Nothing, "never echo into the captured device");
        assert_eq!(plan(AudioMode::TvOnly, sink, pc, "{spk}"), Plan::Mute);
    }

    #[test]
    fn audio_mode_parses() {
        assert_eq!(AudioMode::parse("tv-only"), Ok(AudioMode::TvOnly));
        assert_eq!(AudioMode::parse("off"), Ok(AudioMode::Off));
        assert_eq!(AudioMode::default(), AudioMode::On);
        assert!(AudioMode::parse("yes").is_err());
    }

    #[test]
    fn caps_audio_flag() {
        let l = Link::default();
        l.on_caps(&json!({"v":1,"caps":["audio"],"decoders":[]}));
        assert_eq!(l.tv_audio.load(SeqCst), 1);
        l.on_caps(&json!({"v":1,"caps":[],"decoders":[]}));
        assert_eq!(l.tv_audio.load(SeqCst), 2);
        l.on_caps(&json!({"v":1}));
        assert_eq!(l.tv_audio.load(SeqCst), 2, "old CAPS without the list: no audio");
    }

    #[test]
    fn start_message_is_the_documented_json() {
        let v: serde_json::Value = serde_json::from_str(&start_json(7)).unwrap();
        assert_eq!(v, json!({"v":1,"codec":"pcm_s16le","rate":48000,"ch":2,"frames":240,"sid":7}));
    }

    #[test]
    fn silence_fills_whole_datagrams_behind_now() {
        let udp = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let rx = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        udp.connect(rx.local_addr().unwrap()).unwrap();
        let link = Link::default();
        let mut tx = Sender::new(&udp, 1, &link, InFormat::new(48_000, 2, Sample::F32, 8, 0).unwrap());
        tx.next_t = 1_000_000;
        tx.fill_silence(1_052_000);
        assert_eq!(link.take_stats().sent, 0, "no time base before the first real packet");
        tx.ahead_us = Some(0);
        tx.fill_silence(1_010_000); // 10 ms: too soon
        assert_eq!(link.take_stats().sent, 0);
        tx.fill_silence(1_052_000); // 52 ms - 15 ms lag = 37 ms -> 7 datagrams (35 ms)
        assert_eq!(link.take_stats().sent, 7);
        assert_eq!(tx.next_t, 1_035_000);
        let mut b = [0u8; 2048];
        let n = rx.recv(&mut b).unwrap();
        assert_eq!(n, 20 + 12 + 960);
        assert_eq!(&b[32..36], &[0, 0, 0, 0], "silence");
        // Loopback stamps 29 ms ahead of the clock: silence follows that time base.
        tx.ahead_us = Some(29_000);
        tx.fill_silence(1_052_000); // = 1_081_000 in the loopback base -> up to 1_066_000
        assert_eq!(tx.next_t, 1_065_000);
    }

    #[test]
    fn loopback_chunks_keep_their_capture_time() {
        let udp = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let rx = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        udp.connect(rx.local_addr().unwrap()).unwrap();
        let link = Link::default();
        let mut tx = Sender::new(&udp, 9, &link, InFormat::new(48_000, 2, Sample::F32, 8, 0).unwrap());
        let data: Vec<u8> = (0..480).flat_map(|_| [0.25f32, -0.25]).flat_map(f32::to_le_bytes).collect();
        tx.chunk(&Chunk { data: Some(&data), frames: 480, t_us: Some(5_000_000), discontinuity: true }, 4_971_000);
        assert_eq!(tx.next_t, 5_010_000);
        assert_eq!(tx.ahead_us, Some(29_000), "loopback stamps run ahead of the read time");
        let s = link.take_stats();
        assert_eq!((s.sent, s.cap_gaps), (2, 1));
        let mut b = [0u8; 2048];
        rx.recv(&mut b).unwrap();
        assert_eq!(&b[8..16], &5_000_000u64.to_le_bytes(), "t_us = WASAPI capture time");
        rx.recv(&mut b).unwrap();
        assert_eq!(&b[8..16], &5_005_000u64.to_le_bytes());
        assert_eq!(&b[24..32], &240u64.to_le_bytes(), "pos");
    }
}
