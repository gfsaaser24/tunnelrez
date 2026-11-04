//! The ffmpeg backend: one ffmpeg process per encoder run.
//!
//! ffmpeg `ddagrab` (D3D11 desktop duplication) -> convert/scale on the GPU -> encoder -> FLV on
//! stdout (exact per-frame sizes, no extra frame of delay) -> [`FfmpegSource`] -> session.

use crate::clock::{self, WallToMono};
use crate::encoder::{self, Plan};
use crate::flv::{FlvReader, Video, VideoParser};
use crate::source::{Au, Event, Source, StopMode};
use crate::tools::Tools;
use crate::winutil;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// pts offset added per forced keyframe (1000 s). FLV ms timestamps are u32, so ~4000 per process.
const IDR_JUMP_MS: i64 = 1_000_000;

#[derive(Clone, Debug, Default)]
pub struct RunOpts {
    pub verbose: bool,
    /// Stop after this many frames (probes).
    pub frames: Option<u32>,
    /// Echo ffmpeg's stderr to ours.
    pub echo_stderr: bool,
}

/// Which clock stamps frames: "PTS" = wall clock taken by the demuxer thread right after ddagrab
/// returns the frame (-`use_wallclock_as_timestamps`), "RTCTIME" = wall clock when the frame reaches
/// setpts in the filter thread (later, after any queueing).
fn ts_var() -> &'static str {
    match std::env::var("TUNNELREZ_TS").as_deref() {
        Ok("rtctime") => "RTCTIME",
        _ => "PTS",
    }
}

/// The ffmpeg command line (after the exe) for plan `p`.
pub fn args(p: &Plan, base_unix: i64, o: &RunOpts) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let push = |v: &mut Vec<String>, a: &[&str]| v.extend(a.iter().map(std::string::ToString::to_string));
    push(&mut v, &["-hide_banner", "-loglevel", if o.verbose { "info" } else { "warning" }, "-nostats"]);
    // Main-loop tick: how fast a keyframe command on stdin is picked up.
    push(&mut v, &["-stats_period", "0.02"]);
    // ddagrab runs as a lavfi *input* (its own D3D11 device on adapter 0 of this process, which
    // the GPU preference of ffmpeg-<pref>.exe decides) so that the -vf graph has an input: ffmpeg
    // only delivers runtime filter commands (our keyframe trigger, see force_idr) to graphs with
    // inputs.
    let src = format!(
        "ddagrab=output_idx={o}:framerate={f}:draw_mouse={m}:dup_frames={d}:output_fmt=bgra",
        o = p.cap.output_idx,
        f = p.pacing.grab_fps,
        m = u8::from(p.draw_mouse),
        d = u8::from(p.pacing.dup_frames),
    );
    push(&mut v, &["-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0"]);
    if ts_var() == "PTS" {
        push(&mut v, &["-use_wallclock_as_timestamps", "1"]);
    }
    push(&mut v, &["-f", "lavfi", "-i", &src]);
    // setpts=RTCTIME-base: each frame's pts is the wall clock right after capture (µs), so the
    // FLV timestamp (ms) tells us the capture time; we map wall clock to our QPC clock.
    // Tuning knob: TUNNELREZ_CONV replaces the convert/scale chain (empty = none).
    let conv = std::env::var("TUNNELREZ_CONV").unwrap_or_else(|_| encoder::convert_filters(p));
    let sel = match p.pacing.select_fps {
        Some(sel_fps) => {
            let thr = 1.0 / f64::from(sel_fps) - 0.5 / f64::from(p.pacing.grab_fps.max(1));
            format!("select=isnan(prev_selected_t)+gte(t-prev_selected_t\\,{thr:.5}),")
        }
        None => String::new(),
    };
    let vf = if conv.is_empty() {
        format!("{sel}settb=1/1000000,setpts={}-{base_unix}", ts_var())
    } else {
        format!("{sel}settb=1/1000000,setpts={}-{base_unix},{conv}", ts_var())
    };
    push(&mut v, &["-vf", &vf, "-an", "-fps_mode", "passthrough"]);
    // Keyframe on the first frame and whenever pts jumps by >= 500 s (force_idr adds 1000 s).
    push(&mut v, &["-force_key_frames", "expr:isnan(prev_forced_t)+gte(t-prev_forced_t,500)"]);
    v.extend(encoder::encoder_args(p));
    // Tuning knob: TUNNELREZ_ENC_EXTRA appends encoder options (space separated; later ones win).
    if let Ok(x) = std::env::var("TUNNELREZ_ENC_EXTRA") {
        v.extend(x.split_whitespace().map(String::from));
    }
    if let Some(n) = o.frames {
        push(&mut v, &["-frames:v", &n.to_string()]);
    }
    push(
        &mut v,
        &[
            "-copyts",
            "-flush_packets",
            "1",
            "-avioflags",
            "direct",
            "-flvflags",
            "no_duration_filesize+no_metadata+no_sequence_end",
            "-f",
            "flv",
            "pipe:1",
        ],
    );
    v
}

#[derive(Debug)]
pub struct FfmpegSource {
    child: Child,
    flv: Option<FlvReader<ChildStdout>>,
    vp: VideoParser,
    base_unix: i64,
    w2m: WallToMono,
    pub spawned_us: u64,
    /// Keyframe commands sent (each adds `IDR_JUMP_MS` to the pts).
    idr_sent: u64,
    /// pts jumps seen in the output so far.
    idr_seen: u64,
    last_ts_ms: Option<u32>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    prio: Option<(i32, Instant, bool)>,
}

/// The line says Media Foundation found no encoder transform (`could not find any MFT`, `MF_E_TOPO_CODEC_NOT_FOUND`).
pub fn is_mft_missing(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    (l.contains("mft") && (l.contains("could not find") || l.contains("no ") || l.contains("failed")))
        || l.contains("0xc00d5212")
}

impl FfmpegSource {
    /// Start ffmpeg for plan `p` through the GPU-preference link `t` provides.
    ///
    /// # Errors
    ///
    /// When there is no ffmpeg.exe, or the process cannot be spawned.
    ///
    /// # Panics
    ///
    /// Only the stderr reader thread can panic, and only if the stderr lock was poisoned.
    pub fn spawn(t: &Tools, p: &Plan, o: &RunOpts) -> Result<FfmpegSource> {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const HIGH_PRIORITY_CLASS: u32 = 0x80;
        let w2m = WallToMono::sample();
        let base_unix = clock::unix_us() - 1_000_000;
        let args = args(p, base_unix, o);
        let exe = t.ffmpeg(p.cap.pref)?;
        if o.verbose {
            eprintln!("[ffmpeg] {} {}", exe.display(), args.join(" "));
        }
        let spawned_us = clock::now_us();
        let mut child = Command::new(exe)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | HIGH_PRIORITY_CLASS)
            .spawn()
            .with_context(|| format!("spawn {}", exe.display()))?;
        if std::env::var("TUNNELREZ_NO_TIMER").is_err() {
            if let Err(e) = winutil::high_res_timer_child(&child) {
                eprintln!("[warn] could not raise ffmpeg timer resolution: {e} (capture fps may be < 60)");
            }
        }
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let err = child.stderr.take().context("ffmpeg stderr is not piped")?;
        let (buf, echo) = (Arc::clone(&stderr), o.echo_stderr);
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(std::result::Result::ok) {
                // echo of our keyframe commands
                if l.trim().is_empty() || l.starts_with("Enter command:") || l.starts_with("Command reply") {
                    continue;
                }
                if echo {
                    eprintln!("[ffmpeg] {l}");
                }
                let mut b = buf.lock().expect("ffmpeg stderr lock poisoned");
                b.push_back(l);
                if b.len() > 40 {
                    b.pop_front();
                }
            }
        });
        let stdout = child.stdout.take().context("ffmpeg stdout is not piped")?;
        // Experiment knob: raise every ffmpeg thread (capture thread included) after start-up.
        let prio = std::env::var("TUNNELREZ_FF_THREAD_PRIO")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(|p| (p, Instant::now(), false));
        Ok(FfmpegSource {
            child,
            flv: Some(FlvReader::new(stdout)),
            vp: VideoParser::new(p.codec),
            base_unix,
            w2m,
            spawned_us,
            idr_sent: 0,
            idr_seen: 0,
            last_ts_ms: None,
            stderr,
            prio,
        })
    }

    /// Capture wall-clock time (µs) of a frame from its FLV timestamp, undoing the forced-IDR jumps.
    fn capture_unix(&mut self, ts_ms: u32) -> i64 {
        if let Some(last) = self.last_ts_ms {
            let d = i64::from(ts_ms) - i64::from(last);
            if d > IDR_JUMP_MS / 2 {
                // d > 0 here, so the quotient is non-negative.
                self.idr_seen += ((d + IDR_JUMP_MS / 2) / IDR_JUMP_MS).cast_unsigned();
            }
        }
        self.last_ts_ms = Some(ts_ms);
        self.base_unix + (i64::from(ts_ms) - self.idr_seen.cast_signed() * IDR_JUMP_MS) * 1000
    }

    /// Last ffmpeg error line(s), for probe reports.
    ///
    /// # Panics
    ///
    /// When the stderr reader thread panicked while holding the lock (poisoned).
    pub fn last_error(&self) -> String {
        let b = self.stderr.lock().expect("ffmpeg stderr lock poisoned");
        let interesting = |l: &&String| {
            let x = l.to_ascii_lowercase();
            (x.contains("error")
                || x.contains("fail")
                || x.contains("cannot")
                || x.contains("not supported")
                || x.contains("invalid"))
                && !x.contains("conversion failed")
                && !x.contains("task finished")
                && !x.contains("terminating thread")
                && !x.contains("nothing was written")
                && !x.contains("non monotonically")
        };
        // Media Foundation found no encoder transform (on Windows without the HEVC Video Extensions for
        // hevc_mf): that line has no "error" in it and later generic errors would hide it, so it wins.
        let mft_missing = b.iter().find(|l| is_mft_missing(l));
        let lines: Vec<&String> = b.iter().filter(interesting).collect();
        let pick = mft_missing.or(lines.first().copied()).or(b.back()).map_or("exited without output", |s| s.as_str());
        // drop the "[h264_nvenc @ 0000...]" prefix
        let t = match pick.rfind("] ") {
            Some(i) if pick.starts_with('[') => &pick[i + 2..],
            _ => pick,
        };
        t.trim().chars().take(110).collect()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Source for FfmpegSource {
    fn next(&mut self) -> Result<Option<Event>> {
        loop {
            let Some(flv) = self.flv.as_mut() else { return Ok(None) };
            let tag = match flv.next_tag() {
                Ok(Some(t)) => t,
                Ok(None) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let arrive = clock::now_us();
            if let Some((p, t0, done)) = self.prio.as_mut() {
                if !*done && t0.elapsed() > Duration::from_millis(500) {
                    *done = true;
                    let n = winutil::set_thread_priorities(self.child.id(), *p);
                    eprintln!("[ffmpeg] set {n} threads to priority {p}");
                }
            }
            if tag.kind != 9 {
                continue;
            }
            let pkt = match self.vp.parse(&tag.data) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[flv] bad video tag: {e}");
                    continue;
                }
            };
            match pkt {
                Video::Config(c) => {
                    if !c.is_empty() {
                        return Ok(Some(Event::Config(c)));
                    }
                }
                Video::Frame { key, data } => {
                    let cap_unix = self.capture_unix(tag.ts_ms);
                    let mut cap = self.w2m.mono(cap_unix);
                    // The FLV ms timestamp rounds to nearest; never claim capture after arrival.
                    if cap > arrive {
                        cap = arrive;
                    }
                    if arrive - cap > 2_000_000 {
                        cap = arrive; // clock mapping broken; fall back
                    }
                    return Ok(Some(Event::Frame(Au { data, cap_us: cap, arrive_us: arrive, key, stages: None })));
                }
                Video::Other => {}
            }
        }
    }

    /// Ask the running ffmpeg for an IDR: change the setpts expression so pts jumps by 1000 s;
    /// `-force_key_frames expr:...gte(t-prev_forced_t,500)` then forces a keyframe (each backend's
    /// forced-IDR option makes it an IDR). ffmpeg polls stdin every `-stats_period`, so this lands
    /// in a frame or two. Returns false if the command could not be sent (caller restarts).
    fn force_idr(&mut self) -> bool {
        if self.idr_sent >= 4000 {
            return false;
        }
        let cmd = format!(
            "csetpts -1 expr {}-{}+{}\n",
            ts_var(),
            self.base_unix,
            (self.idr_sent.cast_signed() + 1) * IDR_JUMP_MS * 1000
        );
        let Some(si) = self.child.stdin.as_mut() else { return false };
        if si.write_all(cmd.as_bytes()).and_then(|()| si.flush()).is_err() {
            return false;
        }
        self.idr_sent += 1;
        true
    }

    fn cpu_time_us(&self) -> Option<u64> {
        winutil::cpu_time_us(&self.child)
    }

    fn exit_reason(&mut self) -> String {
        // ffmpeg may still run (e.g. a bad FLV tag ended `next`); nobody reads its stdout any more,
        // so waiting could block forever. Kill it first if it has not exited.
        let st = if let Ok(Some(s)) = self.child.try_wait() {
            Some(s)
        } else {
            let _ = self.child.kill();
            self.child.wait().ok()
        };
        format!("{st:?}, {}", self.last_error())
    }

    fn stop(mut self: Box<Self>, mode: StopMode) {
        if mode == StopMode::Graceful {
            if let Some(mut si) = self.child.stdin.take() {
                let _ = si.write_all(b"q");
                let _ = si.flush();
            }
            // keep draining stdout so ffmpeg can finish writing
            if let Some(flv) = self.flv.take() {
                std::thread::spawn(move || {
                    let mut so = flv.into_inner();
                    let _ = std::io::copy(&mut so, &mut std::io::sink());
                });
            }
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(1500) {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
