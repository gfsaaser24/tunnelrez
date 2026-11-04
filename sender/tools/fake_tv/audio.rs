//! `fake_tv` audio: the TV's real playout code (`receiver/src/jitter.rs`, compiled in here) fed from UDP,
//! pulled every 4 ms by a simulated output with 10 ms latency. Same PC, same QPC clock as the sender, so the
//! capture -> arrival and capture -> "presented" numbers are true latencies, not estimates.

#[path = "../../../receiver/src/jitter.rs"]
mod jitter;

use jitter::{parse_payload, parse_start, Packet, Playout, Recycle, VideoTrack};
use serde_json::{json, Value};
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tunnelrez::audio::wasapi::{Com, Tone};
use tunnelrez::clock::now_us;
use tunnelrez::num::f32_from_i64;
use tunnelrez::proto::{read_msg, T_AUDIO};
use tunnelrez::stats::{pct, r1};

/// Simulated output: a pull of 192 frames (4 ms) every 4 ms, presented 10 ms later.
const BURST_FRAMES: usize = 192;
const OUT_LATENCY_US: u64 = 10_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().expect("fake_tv audio lock poisoned")
}

/// The current audio stream.
struct Stream {
    peer: IpAddr,
    sid: u32,
    playout: Playout,
    /// Datagrams the playout is done with, freed on the UDP path (as on the TV, where the playout runs on the
    /// audio callback).
    recycle: std::sync::mpsc::Receiver<Packet>,
    /// Capture -> arrival (ms), this second and the whole run.
    net_ms: Vec<f32>,
    net_all: Vec<f32>,
    /// Capture -> presented (ms) of the pulls, whole run.
    e2e_all: Vec<f32>,
    pkts: u64,
}

/// Audio state of `fake_tv` (one stream at a time).
#[derive(Default)]
pub(crate) struct Audio {
    stream: Mutex<Option<Stream>>,
    /// The video delay (receive - capture, µs, one sample per second) for A/V sync: like the TV, warm-up left
    /// out, median of a full window.
    video: Mutex<Option<VideoTrack>>,
}

impl Audio {
    /// `AUDIO_START` from `peer`.
    pub(crate) fn start(&self, peer: IpAddr, payload: &[u8]) -> Result<u32, String> {
        let sid = parse_start(payload)?;
        let (tx, recycle) = std::sync::mpsc::sync_channel(512);
        *lock(&self.stream) = Some(Stream {
            peer,
            sid,
            playout: Playout::with_recycle(Recycle(Box::new(move |p| {
                let _ = tx.try_send(p);
            }))),
            recycle,
            net_ms: Vec::new(),
            net_all: Vec::new(),
            e2e_all: Vec::new(),
            pkts: 0,
        });
        Ok(sid)
    }

    /// `AUDIO_STOP` / end of session. Prints the run's audio summary.
    pub(crate) fn stop(&self) {
        if let Some(mut s) = lock(&self.stream).take() {
            let net = pct(&mut s.net_all);
            let e2e = pct(&mut s.e2e_all);
            let t = s.playout.snapshot().totals;
            eprintln!(
                "[fake_tv] audio summary: capture->arrival ms p50={:.2} p99={:.2} max={:.2}; capture->presented ms \
                 p50={:.1} p99={:.1}; underruns={} drops={} lost={} skip_frames={} insert_frames={} slips={}",
                net.0,
                net.1,
                net.2,
                e2e.0,
                e2e.1,
                t.underruns,
                t.drops,
                t.lost,
                t.skip_frames,
                t.insert_frames,
                t.slips
            );
        }
    }

    /// No video this second: video timing starts over (like the TV when no frame was shown for 2 s).
    pub(crate) fn video_clear(&self) {
        if let Some(v) = lock(&self.video).as_mut() {
            v.clear();
        }
    }

    /// One video delay sample (µs) per second.
    pub(crate) fn video_sample(&self, v: i64) {
        lock(&self.video).get_or_insert_with(|| VideoTrack::new(2, 2)).push(v);
    }

    /// Add the TV's audio STATS keys (plus `a_net_ms`: capture -> arrival p50, exact here) to `js`.
    pub(crate) fn stats(&self, js: &mut Value, secs: f32) {
        let mut g = lock(&self.stream);
        let Some(s) = g.as_mut() else { return };
        let snap = s.playout.snapshot();
        let t = snap.totals;
        let (n50, _, _) = pct(&mut s.net_ms);
        s.net_ms.clear();
        let ms = |frames: u64| frames * 1000 / 48_000;
        let extra = json!({
            "a_buf_ms": r1(snap.buf_ms), "a_target_ms": r1(snap.target_ms), "a_delay_ms": r1(snap.delay_ms),
            "a_out_ms": r1(tunnelrez::num::f32_from_u64(OUT_LATENCY_US) / 1000.0),
            "a_sync": if snap.video_sync { "video" } else { "audio" }, "a_mode": "fake",
            "a_pkts_s": r1(tunnelrez::num::f32_from_u64(std::mem::take(&mut s.pkts)) / secs),
            "a_underruns": t.underruns, "a_drops": t.drops, "a_lost": t.lost,
            "a_skip_ms": ms(t.skip_frames), "a_insert_ms": ms(t.insert_frames), "a_slips": t.slips, "a_adjust_ms": ms(t.adjust_frames), "a_xruns": 0,
            "a_net_ms": r1(n50),
        });
        if let (Value::Object(m), Value::Object(e)) = (js, extra) {
            m.extend(e);
            if let Some((st, tv)) = snap.play {
                m.insert("a_play_t_us".into(), st.into());
                m.insert("a_play_us".into(), tv.into());
            }
        }
    }

    /// One datagram from `from`.
    fn datagram(&self, from: IpAddr, datagram: &[u8]) {
        let now = now_us();
        let Ok((hdr, payload)) = read_msg(&mut &datagram[..]) else { return };
        if hdr.typ != T_AUDIO {
            eprintln!("[fake_tv] PROTOCOL ERROR: UDP datagram of type {}", hdr.typ);
            return;
        }
        let Some((sid, pkt)) = parse_payload(hdr.t_us, &payload) else {
            eprintln!("[fake_tv] PROTOCOL ERROR: bad AUDIO datagram ({} bytes)", datagram.len());
            return;
        };
        let mut guard = lock(&self.stream);
        let Some(s) = guard.as_mut().filter(|s| s.peer == from && s.sid == sid) else { return };
        let net = f32_from_i64(now.wrapping_sub(pkt.t_us).cast_signed()) / 1000.0;
        s.net_ms.push(net);
        s.net_all.push(net);
        s.pkts += 1;
        while s.recycle.try_recv().is_ok() {}
        s.playout.push(pkt, now);
    }

    /// One simulated output pull.
    fn pull(&self, buf: &mut [i16]) {
        let video = lock(&self.video).as_ref().and_then(VideoTrack::value);
        let mut g = lock(&self.stream);
        let Some(s) = g.as_mut() else { return };
        let now = now_us();
        s.playout.set_video(video);
        s.playout.pull(buf, now + OUT_LATENCY_US, now);
        if let Some((st, tv)) = s.playout.snapshot().play {
            s.e2e_all.push(f32_from_i64(tv.wrapping_sub(st).cast_signed()) / 1000.0);
        }
    }
}

/// UDP receive thread for `listen` (the TCP listen address).
pub(crate) fn udp_thread(audio: Arc<Audio>, listen: &str) -> std::io::Result<()> {
    let sock = UdpSocket::bind(listen)?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                audio.datagram(from.ip(), &buf[..n]);
            }
        }
    });
    Ok(())
}

/// The simulated audio output of one session, until `done`.
pub(crate) fn output_thread(audio: &Audio, done: &AtomicBool) {
    let mut buf = vec![0i16; BURST_FRAMES * 2];
    let mut next = now_us();
    while !done.load(SeqCst) {
        audio.pull(&mut buf);
        next += 4_000;
        let now = now_us();
        if next > now {
            std::thread::sleep(Duration::from_micros(next - now));
        }
    }
}

/// `--tone`: a quiet 440 Hz tone on the PC's default device while the session runs, so the sender's loopback
/// carries real audio. `amplitude` 0 (`--silent-tone`) plays digital silence: packets flow, nothing is heard.
pub(crate) fn tone_thread(done: &AtomicBool, amplitude: f32) {
    let Ok(_com) = Com::init() else { return };
    let mut t = match Tone::start(amplitude) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[fake_tv] tone: {e:#}");
            return;
        }
    };
    while !done.load(SeqCst) {
        let _ = t.fill();
        std::thread::sleep(Duration::from_millis(5));
    }
}
