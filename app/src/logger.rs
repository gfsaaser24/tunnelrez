//! The logger behind the panel's Log switch.
//!
//! Once a second, for at most [`MAX_SECS`], it records everything the app can read: the settings (picked and
//! running), this PC (sender stage times, encoder fps, sent rate, GPU, CPU), the TV's STATS exactly as received,
//! the network (round trip, latency, Wi-Fi) and events (start, stop, reconnect, setting changes, errors). It runs
//! with or without a stream; without one the TV side is "not connected". Each start writes a new file,
//! `%APPDATA%\TunnelRez\logs\<date>_<time>.md|json`. No IP addresses are written: the TV address is left out and
//! every string is scrubbed.
//!
//! This module has no threads and reads no clock: the controller (`core.rs`) feeds it samples and events.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::platform::{GpuReading, LocalTime, PcReading, Wifi};
use crate::settings::{self, AudioMode, Bitrate, Codec, Encoder, LogFormat, Mode, Picture, Settings};
use crate::state::{Failure, Phase, State};

/// A log stops on its own after this many seconds (one sample per second).
pub(crate) const MAX_SECS: u64 = 120;
/// The file's layout name and version (the run book importer checks it).
pub(crate) const LAYOUT: &str = "tunnelrez-log/1";
/// A sender stats line older than this is not "now" (the sender writes one per second).
pub(crate) const STALE_SECS: u64 = 3;
/// The file is rewritten every this many samples (and at the end), so a crash loses little.
pub(crate) const SAVE_EVERY: usize = 5;
/// A run book entry needs at least this many seconds with the TV connected (the run book's method: 60 s runs).
pub(crate) const MIN_SECS: usize = 60;
/// The first seconds of a stream are start-up; the motion check leaves them out (as the run book does).
const SKIP_SECS: usize = 2;
/// Native NVENC `new_fps` at or above this: the picture moves (the test page draws 60 per second).
const MOVING_FPS: f64 = 30.0;
/// ... below this: it stands still (a static screen repeats the last picture).
const STILL_FPS: f64 = 5.0;
/// Seconds standing still (in a log that also moves) before the content counts as changed; one slow second is not.
const STILL_SECS: usize = 3;

/// Keys of a sender stats line that describe this PC. The rest of the line is the TV's STATS (merged by the
/// sender while they are fresh) and [`NET_KEYS`].
const PC_KEYS: [&str; 23] = [
    "enc_fps",
    "new_fps",
    "send_mbps",
    "cap_iv_ms",
    "enc_ms_p50",
    "enc_ms_p99",
    "cap2tx_ms_p50",
    "cap2tx_ms_p99",
    "tx_ms_p99",
    "tx_ms_max",
    "kf",
    "idr_req",
    "restarts",
    "idr_ms",
    "cap_ms",
    "csc_ms",
    "wait_ms",
    "nvenc_ms",
    "enc_cpu_cores",
    "codec_out",
    "send_size",
    "target_mbps",
    "fixed_rate",
];
/// Keys of a sender stats line about the link between the PC and the TV.
const NET_KEYS: [&str; 4] = ["rtt_ms", "rtt_ms_last", "e2e_ms", "e2e_src"];
/// Keys the sender adds about audio (absent when audio is off, or with an old sender). They are not the TV's
/// STATS: they must not make the TV look connected. The TV's own `a_*` keys stay in its STATS as received, and
/// [`AudioView`] reads them too.
const AUDIO_SENDER_KEYS: [&str; 9] = [
    "audio",
    "audio_tv_only",
    "a_sent_s",
    "a_cap_gaps",
    "audio_e2e_ms",
    "av_ms",
    "audio_dev",
    "audio_route",
    "audio_pc",
];
/// The TV's STATS keys about its Wi-Fi (none yet: the receiver does not report it). Any key that starts with
/// `wifi` or `link_`, or holds `rssi`, counts.
fn is_tv_wifi_key(k: &str) -> bool {
    k.starts_with("wifi") || k.starts_with("link_") || k.contains("rssi")
}

// ------------------------------------------------------------------------------------------------ one sample

/// The settings as picked, and what the sender really runs.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct SettingsView {
    pub(crate) mode: Mode,
    pub(crate) picture: Picture,
    /// The picked size, `1920x1080`.
    pub(crate) resolution: String,
    pub(crate) fps: u32,
    pub(crate) codec: Codec,
    pub(crate) encoder: Encoder,
    /// The encoder in the panel's words, with the path (`NVIDIA NVENC (FFmpeg)`).
    pub(crate) encoder_label: &'static str,
    /// `"default"` (the picture's own rate), `"auto"`, or the Mb/s the user picked.
    pub(crate) bitrate: Bitrate,
    /// The fixed rate the app asks the sender for, Mb/s; `null` = Auto (the sender's ceiling).
    pub(crate) bitrate_target_mbps: Option<u32>,
    /// The audio setting: `on`, `off` or `tv_only`.
    pub(crate) audio: AudioMode,
    /// The audio device setting: `""` = auto, `off`, or a device name.
    pub(crate) audio_sink: String,
    /// What the sender says it runs; `null` without a stream.
    pub(crate) running: Option<Running>,
}

/// Audio this second, from the sender's stats line (its own keys) and the TV's STATS (`a_*`). `null` for the whole
/// block when neither side said anything about audio (audio off, or an old sender and TV). Every number is
/// `null` when its source did not report it.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct AudioView {
    /// The sender's `audio`: `off`, `waiting`, `tv_no_audio`, `on` or `error`.
    pub(crate) state: Option<String>,
    /// The sender mutes this PC's speakers (Play on TV only).
    pub(crate) tv_only: Option<bool>,
    /// The playback device captured, its friendly name (`audio_dev`).
    pub(crate) device: Option<String>,
    /// `sink` (a virtual device) or `loopback` (the PC speakers) (`audio_route`).
    pub(crate) route: Option<String>,
    /// With a sink: the sound is also played on the PC speakers (`audio_pc`).
    pub(crate) pc: Option<bool>,
    /// Datagrams sent in the last second (the sender's `a_sent_s`).
    pub(crate) sent_per_s: Option<f64>,
    /// WASAPI discontinuities, session total (`a_cap_gaps`).
    pub(crate) cap_gaps: Option<f64>,
    /// Capture to presented on the TV, ms (`audio_e2e_ms`).
    pub(crate) e2e_ms: Option<f64>,
    /// `audio_e2e_ms - e2e_ms`: positive = the sound is behind the picture.
    pub(crate) av_ms: Option<f64>,
    /// Audio received and not yet played, ms (`a_buf_ms`).
    pub(crate) buf_ms: Option<f64>,
    /// The TV's jitter target, ms (`a_target_ms`).
    pub(crate) target_ms: Option<f64>,
    /// The TV's whole added wait, ms (`a_delay_ms`).
    pub(crate) delay_ms: Option<f64>,
    /// `AAudio` output latency, ms (`a_out_ms`).
    pub(crate) out_ms: Option<f64>,
    /// `video` or `audio`: what sets the TV's play delay (`a_sync`).
    pub(crate) sync: Option<String>,
    /// The `AAudio` stream's sharing mode, `exclusive` or `shared` (`a_mode`).
    pub(crate) mode: Option<String>,
    /// Datagrams the TV received in the last second (`a_pkts_s`).
    pub(crate) pkts_per_s: Option<f64>,
    /// Session totals of the TV: output periods with audio missing, datagrams dropped, datagrams never received.
    pub(crate) underruns: Option<f64>,
    pub(crate) drops: Option<f64>,
    pub(crate) lost: Option<f64>,
}

impl AudioView {
    /// From the sender's audio keys (`sender`) and the TV's STATS (`tv`); `None` when there is nothing.
    fn of(sender: &Map<String, Value>, tv: &Map<String, Value>) -> Option<Self> {
        let num = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_f64);
        let text = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
        let v = Self {
            state: text(sender, "audio"),
            tv_only: sender.get("audio_tv_only").and_then(Value::as_bool),
            device: text(sender, "audio_dev"),
            route: text(sender, "audio_route"),
            pc: sender.get("audio_pc").and_then(Value::as_bool),
            sent_per_s: num(sender, "a_sent_s"),
            cap_gaps: num(sender, "a_cap_gaps"),
            e2e_ms: num(sender, "audio_e2e_ms"),
            av_ms: num(sender, "av_ms"),
            buf_ms: num(tv, "a_buf_ms"),
            target_ms: num(tv, "a_target_ms"),
            delay_ms: num(tv, "a_delay_ms"),
            out_ms: num(tv, "a_out_ms"),
            sync: text(tv, "a_sync"),
            mode: text(tv, "a_mode"),
            pkts_per_s: num(tv, "a_pkts_s"),
            underruns: num(tv, "a_underruns"),
            drops: num(tv, "a_drops"),
            lost: num(tv, "a_lost"),
        };
        (v != Self::default()).then_some(v)
    }
}

/// What the sender really runs, from its stats line.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct Running {
    /// As the sender writes it: `hevc/native`, `h264/nvenc zero-copy`.
    pub(crate) codec_out: Option<String>,
    /// In the panel's words: `HEVC · NVIDIA NVENC native`.
    pub(crate) path: Option<String>,
    /// The size really encoded: `3840x2160@60`.
    pub(crate) size: Option<String>,
    /// The rate the encoder was given, Mb/s.
    pub(crate) target_mbps: Option<f64>,
    /// The sender pads up to the target (a fixed rate), rather than treating it as a ceiling.
    pub(crate) fixed_rate: Option<bool>,
    /// What Auto picked, when the encoder or the codec is Auto (`path`).
    pub(crate) auto_picked: Option<String>,
}

/// Sender stage times this second, ms (median unless named). `null` where the path does not measure it: capture
/// and convert are only timed by the native NVENC path, and only on new pictures (a second of a static screen,
/// where every frame re-encodes the last picture, has them `null`).
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct Stages {
    pub(crate) capture: Option<f64>,
    pub(crate) convert: Option<f64>,
    pub(crate) encode: Option<f64>,
    /// Native NVENC: waiting for the encoded frame.
    pub(crate) encode_wait: Option<f64>,
    /// Writing a frame to the socket, 99th percentile.
    pub(crate) send_p99: Option<f64>,
    /// Capture to the end of sending.
    pub(crate) capture_to_send: Option<f64>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct PcView {
    pub(crate) stages_ms: Stages,
    pub(crate) encoder_fps: Option<f64>,
    /// Native NVENC: new pictures per second (the rest of `encoder_fps` repeats the last one).
    pub(crate) new_fps: Option<f64>,
    pub(crate) sent_mbps: Option<f64>,
    pub(crate) cpu_pct: Option<f64>,
    pub(crate) gpu: Option<GpuReading>,
    /// Every PC key of the sender's stats line, as written.
    pub(crate) sender: Map<String, Value>,
}

/// The TV's side: its STATS exactly as received, or "not connected".
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub(crate) enum Tv {
    Stats(Map<String, Value>),
    NotConnected(&'static str),
}

pub(crate) const NOT_CONNECTED: &str = "not connected";
/// `tv_latency_source` when the TV's STATS carry `lat_ms_p50`.
pub(crate) const TV_LATENCY_TV: &str = "tv lat_ms_p50";
/// `tv_latency_source` when it is worked out: the exact capture-to-shown latency (TV clock) minus capture-to-sent
/// on this PC and half the round trip. The receiver logs `lat_ms_p50` to logcat but does not send it.
pub(crate) const TV_LATENCY_EST: &str = "est: latency (tv_clock) - capture_to_send - rtt/2";
pub(crate) const TV_WIFI_MISSING: &str = "not reported by the TV";

/// The TV's Wi-Fi keys, or why there are none.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub(crate) enum TvWifi {
    Reported(Map<String, Value>),
    Missing(&'static str),
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct NetView {
    /// Round trip to the TV, ms: the best of the last 20 s, and the last one.
    pub(crate) rtt_ms: Option<f64>,
    pub(crate) rtt_ms_last: Option<f64>,
    /// Capture to display, ms, and how it was measured (`tv_clock` = exact).
    pub(crate) latency_ms: Option<f64>,
    pub(crate) latency_source: Option<String>,
    /// The TV's own part (frame received to shown), ms: its `lat_ms_p50` when its STATS carry it, else
    /// worked out from the exact latency ([`TV_LATENCY_EST`]). `null` without an exact latency.
    pub(crate) tv_latency_ms: Option<f64>,
    pub(crate) tv_latency_source: Option<&'static str>,
    pub(crate) pc_wifi: Wifi,
    pub(crate) tv_wifi: TvWifi,
}

/// Everything readable in one second.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct Sample {
    /// Seconds since the log started.
    pub(crate) t_s: u64,
    /// The app's state in words: `idle`, `streaming`, `connecting (reconnecting)`.
    pub(crate) state: String,
    /// What was on the TV screen when the app knows it: the built-in test page while it fills the TV screen.
    /// `null` = not known (anything the user had there).
    pub(crate) content: Option<String>,
    pub(crate) settings: SettingsView,
    pub(crate) pc: PcView,
    pub(crate) tv: Tv,
    pub(crate) network: NetView,
    /// Audio this second; `null` when neither the sender nor the TV reported any.
    pub(crate) audio: Option<AudioView>,
}

/// The app's state in short words for the log.
pub(crate) fn state_words(state: &State) -> String {
    let phase = |p: &Phase| match p {
        Phase::Checking => "checking",
        Phase::Plugging => "plugging",
        Phase::Starting => "starting",
        Phase::Reconnecting => "reconnecting",
        Phase::Restarting => "restarting",
    };
    match state {
        State::FirstRun => "first run".into(),
        State::Idle => "idle".into(),
        State::Connecting { phase: p, .. } => format!("connecting ({})", phase(p)),
        State::Streaming { .. } => "streaming".into(),
        State::Stopping => "stopping".into(),
        State::Error { error } => format!("error ({})", failure_words(error)),
    }
}

/// A failure in words, without the TV address.
pub(crate) fn failure_words(f: &Failure) -> String {
    match f {
        Failure::TvUnreachable { .. } => "the TV did not answer".into(),
        Failure::TvAppNotOpen { .. } => "the TV app is not open".into(),
        Failure::SenderMissing => "the sender is missing".into(),
        Failure::MonitorFailed { message, .. } => format!("virtual monitor: {}", redact_ips(message)),
        Failure::MonitorModeFailed { width, height } => {
            format!("could not switch the TV monitor to {width}x{height}")
        }
        Failure::SenderFailed { message } => format!("sender: {}", redact_ips(message)),
        Failure::Unsupported { message, .. } => format!("unsupported: {}", redact_ips(message)),
    }
}

/// Build one sample. `line` is the sender's latest stats line while it is fresh (`None` without a stream);
/// `content` is what the app knows is on the TV screen (the test page), if anything.
pub(crate) fn sample(
    t_s: u64,
    state: &State,
    settings: &Settings,
    line: Option<&Value>,
    pc: PcReading,
    content: Option<&str>,
) -> Sample {
    let mut line = line.and_then(Value::as_object).cloned().unwrap_or_default();
    for v in line.values_mut() {
        scrub(v);
    }
    let mut pc_keys = Map::new();
    let mut net = Map::new();
    let mut tv = Map::new();
    let mut audio_keys = Map::new();
    for (k, v) in line {
        if PC_KEYS.contains(&k.as_str()) {
            pc_keys.insert(k, v);
        } else if NET_KEYS.contains(&k.as_str()) {
            net.insert(k, v);
        } else if AUDIO_SENDER_KEYS.contains(&k.as_str()) {
            audio_keys.insert(k, v);
        } else {
            tv.insert(k, v);
        }
    }
    let num = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_f64);
    // `[p50, p99]` (native stage times) -> p50.
    let p50 = |m: &Map<String, Value>, k: &str| m.get(k).and_then(|v| v.get(0)).and_then(Value::as_f64);
    let stages = Stages {
        capture: p50(&pc_keys, "cap_ms"),
        convert: p50(&pc_keys, "csc_ms"),
        encode: p50(&pc_keys, "nvenc_ms").or_else(|| num(&pc_keys, "enc_ms_p50")),
        encode_wait: p50(&pc_keys, "wait_ms"),
        send_p99: num(&pc_keys, "tx_ms_p99"),
        capture_to_send: num(&pc_keys, "cap2tx_ms_p50"),
    };
    let codec_out = pc_keys.get("codec_out").and_then(Value::as_str).map(str::to_string);
    let running = (!pc_keys.is_empty()).then(|| {
        let path = codec_out.as_deref().map(settings::codec_out_words);
        let auto = settings.encoder == Encoder::Auto || settings.codec == Codec::Auto;
        Running {
            auto_picked: path.clone().filter(|_| auto),
            path,
            codec_out,
            size: pc_keys.get("send_size").and_then(Value::as_str).map(str::to_string),
            target_mbps: num(&pc_keys, "target_mbps"),
            fixed_rate: pc_keys.get("fixed_rate").and_then(Value::as_bool),
        }
    });
    let capture_to_send = stages.capture_to_send;
    let (w, h) = settings.picture.size();
    let tv_wifi: Map<String, Value> =
        tv.iter().filter(|(k, _)| is_tv_wifi_key(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let tv_wifi_view = if tv_wifi.is_empty() { TvWifi::Missing(TV_WIFI_MISSING) } else { TvWifi::Reported(tv_wifi) };
    let network = net_view(&net, &tv, capture_to_send, pc.wifi, tv_wifi_view);
    let audio = AudioView::of(&audio_keys, &tv);
    // The sender merges the TV's STATS only while they are fresh: any TV key means the TV is reporting.
    let tv_view = if tv.is_empty() { Tv::NotConnected(NOT_CONNECTED) } else { Tv::Stats(tv) };
    Sample {
        t_s,
        state: state_words(state),
        content: content.map(str::to_string),
        settings: SettingsView {
            mode: settings.mode,
            picture: settings.picture,
            resolution: format!("{w}x{h}"),
            fps: Picture::FPS,
            codec: settings.codec,
            encoder: settings.encoder,
            encoder_label: settings.encoder.label(),
            bitrate: settings.bitrate_mbps,
            bitrate_target_mbps: settings.bitrate_mbps.effective(settings.picture),
            audio: settings.audio,
            audio_sink: settings.audio_sink.clone(),
            running,
        },
        pc: PcView {
            stages_ms: stages,
            encoder_fps: num(&pc_keys, "enc_fps"),
            new_fps: num(&pc_keys, "new_fps"),
            sent_mbps: num(&pc_keys, "send_mbps"),
            cpu_pct: pc.cpu_pct,
            gpu: pc.gpu,
            sender: pc_keys,
        },
        network,
        tv: tv_view,
        audio,
    }
}

/// The network part of a sample from the sender's link keys (`net`), the TV's STATS (`tv`) and the Wi-Fi.
fn net_view(
    net: &Map<String, Value>,
    tv: &Map<String, Value>,
    capture_to_send: Option<f64>,
    pc_wifi: Wifi,
    tv_wifi: TvWifi,
) -> NetView {
    let num = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_f64);
    let latency_ms = num(net, "e2e_ms");
    let latency_source = net.get("e2e_src").and_then(Value::as_str).map(str::to_string);
    let rtt_ms = num(net, "rtt_ms");
    let (tv_latency_ms, tv_latency_source) =
        tv_latency(num(tv, "lat_ms_p50"), latency_ms, latency_source.as_deref(), capture_to_send, rtt_ms);
    NetView {
        rtt_ms,
        rtt_ms_last: num(net, "rtt_ms_last"),
        latency_ms,
        latency_source,
        tv_latency_ms,
        tv_latency_source,
        pc_wifi,
        tv_wifi,
    }
}

/// The TV's part of the latency (see [`NetView::tv_latency_ms`]).
fn tv_latency(
    tv_reported: Option<f64>,
    latency_ms: Option<f64>,
    latency_source: Option<&str>,
    capture_to_send: Option<f64>,
    rtt_ms: Option<f64>,
) -> (Option<f64>, Option<&'static str>) {
    if let Some(l) = tv_reported {
        return (Some(l), Some(TV_LATENCY_TV));
    }
    match (latency_ms, latency_source, capture_to_send, rtt_ms) {
        (Some(e2e), Some("tv_clock"), Some(c2s), Some(rtt)) => {
            let est = ((e2e - c2s - rtt / 2.0) * 10.0).round() / 10.0;
            (Some(est.max(0.0)), Some(TV_LATENCY_EST))
        }
        _ => (None, None),
    }
}

// ------------------------------------------------------------------------------------------------ events

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    LogStart,
    LogStop,
    /// A stream start was asked for.
    StreamStart,
    /// Frames reach the TV.
    Connected,
    Reconnecting,
    /// The sender exited and is started again.
    Restarting,
    /// A stop was asked for.
    StreamStop,
    /// The stream has stopped and everything is cleaned up.
    Stopped,
    /// A setting changed.
    Setting,
    Error,
    /// A warning that does not stop the stream (encoder fallback).
    Notice,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct LogEvent {
    /// Seconds since the log started, 0.1 s steps.
    pub(crate) t_s: f64,
    pub(crate) kind: EventKind,
    pub(crate) detail: String,
}

/// The event for a state change, if it is one worth logging.
pub(crate) fn state_event(from: &State, to: &State) -> Option<(EventKind, String)> {
    let words = state_words(to);
    Some(match (from, to) {
        (_, State::Connecting { phase: Phase::Checking, .. }) => (EventKind::StreamStart, words),
        (_, State::Connecting { phase: Phase::Reconnecting, .. }) => (EventKind::Reconnecting, words),
        (_, State::Connecting { phase: Phase::Restarting, .. }) => (EventKind::Restarting, words),
        (_, State::Streaming { .. }) => (EventKind::Connected, words),
        (_, State::Stopping) => (EventKind::StreamStop, words),
        (State::Stopping, State::Idle) => (EventKind::Stopped, words),
        (_, State::Error { error }) => (EventKind::Error, failure_words(error)),
        _ => return None,
    })
}

/// Why a log ended.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    /// The user switched Log off.
    ByHand,
    /// [`MAX_SECS`] passed.
    TimeLimit,
    /// The app quit (or Windows is shutting down).
    AppClosed,
    /// The built-in test run ended (stopped, or the stream stopped) before the time limit.
    TestEnded,
}

impl StopReason {
    fn words(self) -> &'static str {
        match self {
            Self::ByHand => "stopped by hand",
            Self::TimeLimit => "stopped after 2 minutes",
            Self::AppClosed => "the app closed",
            Self::TestEnded => "the test run ended",
        }
    }
}

/// What makes a log a poor run book entry, worked out from its samples. The panel shows `warnings`; the run book
/// importer marks the run.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Checks {
    /// Seconds with the TV connected (the seconds a run is made of).
    pub(crate) tv_seconds: usize,
    /// Fewer than [`MIN_SECS`] of them.
    pub(crate) short: bool,
    /// What was on screen the whole time, when the app knows it (`motion60 test page`).
    pub(crate) content: Option<String>,
    /// What was on screen changed: the test page came or went, or the picture stopped moving for a while.
    pub(crate) content_changed: bool,
    /// The running codec, size or rate changed (the importer keeps the longest stretch with one setting).
    pub(crate) settings_changed: bool,
    /// The above in plain words.
    pub(crate) warnings: Vec<String>,
}

/// The checks of `samples` (see [`Checks`]).
fn checks_of(samples: &[Sample]) -> Checks {
    let tv: Vec<&Sample> = samples.iter().filter(|s| matches!(s.tv, Tv::Stats(_))).collect();
    let mut c = Checks { tv_seconds: tv.len(), short: tv.len() < MIN_SECS, ..Checks::default() };
    let mut warnings = Vec::new();
    if c.short && !tv.is_empty() {
        warnings.push(format!(
            "Short log: {} s with the TV connected. A run needs at least {MIN_SECS} s; record it again.",
            tv.len()
        ));
    }
    let first = tv.first().and_then(|s| s.content.clone());
    let page_changed = tv.iter().any(|s| s.content != first);
    c.content = first.filter(|_| !page_changed);
    // Native NVENC counts new pictures: a moving page gives ~60 a second, a static screen ~0.
    let new_fps: Vec<f64> = tv.iter().skip(SKIP_SECS).filter_map(|s| s.pc.new_fps).collect();
    let still = new_fps.iter().filter(|&&f| f < STILL_FPS).count();
    let stopped_moving = still >= STILL_SECS && new_fps.iter().any(|&f| f >= MOVING_FPS);
    c.content_changed = page_changed || stopped_moving;
    if page_changed {
        warnings.push("The test page was not on the TV screen for the whole log.".into());
    } else if stopped_moving {
        warnings.push(format!("The picture stopped moving for {still} s during the log."));
    }
    let key = |s: &Sample| {
        s.settings
            .running
            .as_ref()
            .map(|r| (r.codec_out.clone(), r.size.clone(), r.target_mbps.map(f64::to_bits), s.settings.audio))
    };
    let mut keys = tv.iter().filter_map(|s| key(s));
    if let Some(k0) = keys.next() {
        c.settings_changed = keys.any(|k| k != k0);
    }
    if c.settings_changed {
        warnings.push("The stream settings changed during the log; the run book keeps the longest stretch.".into());
    }
    c.warnings = warnings;
    c
}

// ------------------------------------------------------------------------------------------------ recording

/// One log: what it has recorded so far, and where it goes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recording {
    path: PathBuf,
    format: LogFormat,
    started_at: LocalTime,
    app_version: &'static str,
    /// File name of the sender in use.
    sender: Option<String>,
    samples: Vec<Sample>,
    events: Vec<LogEvent>,
    stop: Option<StopReason>,
}

/// The JSON document. Units are in the key names (`_ms`, `_pct`, `_mbps`, `_mhz`, `_w`, `_c`, `_dbm`); a value that
/// could not be read is `null`.
#[derive(Serialize)]
struct Doc<'a> {
    layout: &'static str,
    app_version: &'static str,
    sender: Option<&'a str>,
    /// Local time the log started.
    started: String,
    interval_s: u64,
    max_s: u64,
    seconds: usize,
    stopped: Option<StopReason>,
    /// Sources that gave nothing during this log, in words.
    missing: Vec<String>,
    /// Short log, content or settings changed (see [`Checks`]).
    checks: Checks,
    events: &'a [LogEvent],
    samples: &'a [Sample],
}

impl Recording {
    pub(crate) fn new(path: PathBuf, format: LogFormat, started_at: LocalTime, sender: Option<String>) -> Self {
        Self {
            path,
            format,
            started_at,
            app_version: env!("CARGO_PKG_VERSION"),
            sender,
            samples: Vec::new(),
            events: Vec::new(),
            stop: None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn format(&self) -> LogFormat {
        self.format
    }

    pub(crate) fn seconds(&self) -> usize {
        self.samples.len()
    }

    #[cfg(test)]
    fn events(&self) -> &[LogEvent] {
        &self.events
    }

    /// Record an event `elapsed_s` seconds after the start. The detail is scrubbed of IP addresses.
    pub(crate) fn event(&mut self, elapsed_s: f64, kind: EventKind, detail: &str) {
        let t_s = (elapsed_s * 10.0).round() / 10.0;
        self.events.push(LogEvent { t_s, kind, detail: redact_ips(detail) });
    }

    pub(crate) fn push(&mut self, s: Sample) {
        self.samples.push(s);
    }

    /// End the log: no more samples or events count after this.
    pub(crate) fn finish(&mut self, elapsed_s: f64, reason: StopReason) {
        if self.stop.is_none() {
            self.event(elapsed_s, EventKind::LogStop, reason.words());
            self.stop = Some(reason);
        }
    }

    #[cfg(test)]
    fn is_finished(&self) -> bool {
        self.stop.is_some()
    }

    /// Short log, content or settings changed (see [`Checks`]).
    pub(crate) fn checks(&self) -> Checks {
        checks_of(&self.samples)
    }

    /// The file's text in its format.
    pub(crate) fn render(&self) -> String {
        match self.format {
            LogFormat::Markdown => self.to_markdown(),
            LogFormat::Json => self.to_json(),
        }
    }

    /// Sources that gave nothing in any sample, in words.
    fn missing(&self) -> Vec<String> {
        let mut out = Vec::new();
        let s = &self.samples;
        if s.is_empty() {
            return out;
        }
        if s.iter().all(|x| matches!(x.tv, Tv::NotConnected(_))) {
            out.push("TV: not connected during this log".into());
        } else if s.iter().all(|x| matches!(x.network.tv_wifi, TvWifi::Missing(_))) {
            out.push("TV Wi-Fi: not reported by the TV yet".into());
        }
        if s.iter().all(|x| x.pc.gpu.is_none()) {
            out.push("GPU: no NVIDIA readings (NVML and nvidia-smi gave nothing)".into());
        }
        if s.iter().all(|x| x.pc.cpu_pct.is_none()) {
            out.push("CPU: no readings".into());
        }
        if let Some(Wifi::Unavailable { reason }) =
            s.iter().map(|x| &x.network.pc_wifi).find(|w| matches!(w, Wifi::Unavailable { .. }))
        {
            out.push(format!("PC Wi-Fi: {reason}"));
        }
        let ran = s.iter().any(|x| x.settings.running.is_some());
        let native =
            s.iter().filter_map(|x| x.settings.running.as_ref()?.codec_out.as_deref()).any(|c| c.contains("/native"));
        if ran && s.iter().all(|x| x.pc.stages_ms.capture.is_none()) {
            out.push(if native && s.iter().any(|x| x.pc.stages_ms.convert.is_some()) {
                // The sender leaves a frame's capture time out when the capture API's time is not before it picked
                // the frame up (WGC at 1080p): unknown, not 0.
                "Capture time: the capture API gave no frame time before pick-up, so it is unknown".into()
            } else {
                "Capture and convert times: only the native NVENC path measures them".into()
            });
        }
        out
    }

    pub(crate) fn to_json(&self) -> String {
        let doc = Doc {
            layout: LAYOUT,
            app_version: self.app_version,
            sender: self.sender.as_deref(),
            started: format!("{}T{}", date(self.started_at), time(self.started_at, ':')),
            interval_s: 1,
            max_s: MAX_SECS,
            seconds: self.samples.len(),
            stopped: self.stop,
            missing: self.missing(),
            checks: self.checks(),
            events: &self.events,
            samples: &self.samples,
        };
        serde_json::to_string_pretty(&doc).expect("the log serialises (plain data)")
    }

    pub(crate) fn to_markdown(&self) -> String {
        let mut o = String::new();
        self.md_header(&mut o);
        self.md_events(&mut o);
        self.md_table(&mut o);
        self.md_values(&mut o);
        o
    }

    fn md_header(&self, o: &mut String) {
        let at = format!("{} {}", date(self.started_at), time(self.started_at, ':'));
        let _ = writeln!(o, "# TunnelRez log, {at}\n");
        let stopped = self.stop.map_or("still running", StopReason::words);
        let _ = writeln!(o, "- {} s, one sample per second ({stopped})", self.samples.len());
        let _ = writeln!(
            o,
            "- App {}{}; layout `{LAYOUT}`",
            self.app_version,
            self.sender.as_deref().map(|s| format!(", sender {s}")).unwrap_or_default()
        );
        if let Some(first) = self.samples.first() {
            let st = &first.settings;
            let rate = match (st.bitrate, st.bitrate_target_mbps) {
                (Bitrate::Auto, _) => "Auto (the sender's ceiling)".to_string(),
                (Bitrate::Default, Some(v)) => format!("{v} Mb/s (the picture's default)"),
                (_, Some(v)) => format!("{v} Mb/s (picked)"),
                (_, None) => "–".into(),
            };
            let _ = writeln!(
                o,
                "- Settings at the start: {} · {} · codec {} · encoder {} · bitrate {rate} · audio {}",
                word(&st.mode),
                st.picture.label(),
                word(&st.codec),
                st.encoder_label,
                word(&st.audio)
            );
        }
        for m in self.missing() {
            let _ = writeln!(o, "- Missing: {m}");
        }
        let checks = self.checks();
        if let Some(c) = &checks.content {
            let _ = writeln!(o, "- On screen: {c}");
        }
        for w in &checks.warnings {
            let _ = writeln!(o, "- Check: {w}");
        }
        o.push_str("\nUnits: ms, Mb/s, %, MHz, W. `–` = could not be read.\n");
    }

    fn md_events(&self, o: &mut String) {
        o.push_str("\n## Events\n\n| t (s) | Event | Detail |\n|---|---|---|\n");
        for e in &self.events {
            let _ = writeln!(o, "| {:.1} | {} | {} |", e.t_s, word(&e.kind), cell(&e.detail));
        }
    }

    fn md_table(&self, o: &mut String) {
        o.push_str("\n## Every second\n\n");
        o.push_str(
            "| t | State | Running | TV fps | Latency | Decode | Sent / target Mb/s | Capture | Convert | Encode | \
             Send p99 | Enc fps | GPU % | Enc % | Clock MHz | Power W | CPU % | RTT | PC Wi-Fi | TV Wi-Fi | \
             Audio ms | A/V ms | Audio buf |\n",
        );
        o.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for s in &self.samples {
            let tv = match &s.tv {
                Tv::Stats(m) => Some(m),
                Tv::NotConnected(_) => None,
            };
            let tvf = |k: &str| tv.and_then(|m| m.get(k)).and_then(Value::as_f64);
            let run = s.settings.running.as_ref();
            let gpu = s.pc.gpu.as_ref();
            let g = |f: fn(&GpuReading) -> Option<f64>| gpu.and_then(f);
            let target = run.and_then(|r| r.target_mbps);
            let audio = s.audio.as_ref();
            let _ = writeln!(
                o,
                "| {} | {} | {} | {} | {} | {} | {} / {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                s.t_s,
                s.state,
                cell(run.and_then(|r| r.path.as_deref()).unwrap_or("–")),
                n(tvf("fps_out"), 1),
                n(s.network.latency_ms, 1),
                n(tvf("decode_ms_p50"), 1),
                n(s.pc.sent_mbps, 1),
                n(target, 0),
                n(s.pc.stages_ms.capture, 1),
                n(s.pc.stages_ms.convert, 1),
                n(s.pc.stages_ms.encode, 1),
                n(s.pc.stages_ms.send_p99, 1),
                n(s.pc.encoder_fps, 1),
                n(g(|g| g.core_pct), 0),
                n(g(|g| g.encoder_pct), 0),
                n(g(|g| g.clock_mhz), 0),
                n(g(|g| g.power_w), 1),
                n(s.pc.cpu_pct, 0),
                n(s.network.rtt_ms, 1),
                cell(&wifi_words(&s.network.pc_wifi)),
                match &s.network.tv_wifi {
                    TvWifi::Reported(m) => cell(&kv(m)),
                    TvWifi::Missing(_) => "–".into(),
                },
                n(audio.and_then(|a| a.e2e_ms), 1),
                n(audio.and_then(|a| a.av_ms), 1),
                n(audio.and_then(|a| a.buf_ms), 1),
            );
        }
    }

    /// Each second, every value as read.
    fn md_values(&self, o: &mut String) {
        o.push_str("\n## All values\n\nEach second, every value as read (the TV's STATS exactly as received).\n");
        for s in &self.samples {
            let _ = writeln!(o, "\n### {} s\n", s.t_s);
            let _ = writeln!(o, "- **settings**: {}", kv_value(&serde_json::to_value(&s.settings).unwrap_or_default()));
            let mut pc = serde_json::to_value(&s.pc).unwrap_or_default();
            if let Some(m) = pc.as_object_mut() {
                m.remove("sender");
            }
            let _ = writeln!(o, "- **pc**: {}", kv_value(&pc));
            let _ = writeln!(o, "- **pc sender line**: {}", kv(&s.pc.sender));
            match &s.tv {
                Tv::Stats(m) => {
                    let _ = writeln!(o, "- **tv**: {}", kv(m));
                }
                Tv::NotConnected(w) => {
                    let _ = writeln!(o, "- **tv**: {w}");
                }
            }
            let _ = writeln!(o, "- **network**: {}", kv_value(&serde_json::to_value(&s.network).unwrap_or_default()));
            if let Some(a) = &s.audio {
                let _ = writeln!(o, "- **audio**: {}", kv_value(&serde_json::to_value(a).unwrap_or_default()));
            }
        }
    }

    /// Write the file (atomically: a temp file, then a rename over the old one).
    ///
    /// # Errors
    /// When the folder or the file cannot be written.
    #[cfg(test)]
    fn save(&self) -> Result<()> {
        write_atomic(&self.path, &self.render())
    }
}

/// Write `text` to `path` through a temp file and a rename.
///
/// # Errors
/// When the folder or the file cannot be written.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("log path has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

/// `2026-10-26`.
fn date(t: LocalTime) -> String {
    format!("{:04}-{:02}-{:02}", t.year, t.month, t.day)
}

/// `14:03:07` (or `14-03-07` for file names).
fn time(t: LocalTime, sep: char) -> String {
    format!("{:02}{sep}{:02}{sep}{:02}", t.hour, t.minute, t.second)
}

/// `<dir>\2026-10-26_14-03-07.json`, or `..._2.json` when that file exists: every log is a new file.
pub(crate) fn log_path(dir: &Path, at: LocalTime, format: LogFormat) -> PathBuf {
    let stem = format!("{}_{}", date(at), time(at, '-'));
    let ext = format.extension();
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..=u32::MAX).map(|i| dir.join(format!("{stem}_{i}.{ext}"))).find(|p| !p.exists()).expect("some suffix is free")
}

/// Seconds left on the countdown after `elapsed_ms`, rounded up (120 at the start, 0 at the end).
pub(crate) fn remaining_s(elapsed_ms: u128) -> u64 {
    let total = u128::from(MAX_SECS) * 1000;
    u64::try_from(total.saturating_sub(elapsed_ms).div_ceil(1000)).unwrap_or(MAX_SECS)
}

/// An enum in its file form (`extend`, `hevc`).
fn word(v: &impl Serialize) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// A number with `digits` decimals, or `–`.
fn n(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "–".into(), |v| format!("{v:.digits$}"))
}

/// Text safe inside a Markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

/// The PC's Wi-Fi in a few words: `6 GHz · 802.11ax · 1201/1201 Mb/s · 92 %`.
fn wifi_words(w: &Wifi) -> String {
    match w {
        Wifi::Connected(l) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(b) = &l.band {
                parts.push(b.clone());
            }
            if let Some(p) = &l.phy {
                parts.push(p.split(' ').next().unwrap_or(p).to_string());
            }
            if let (Some(rx), Some(tx)) = (l.rx_mbps, l.tx_mbps) {
                parts.push(format!("{rx:.0}/{tx:.0} Mb/s"));
            }
            if let Some(q) = l.signal_pct {
                parts.push(format!("{q} %"));
            }
            if let Some(r) = l.rssi_dbm {
                parts.push(format!("{r} dBm"));
            }
            parts.join(" · ")
        }
        Wifi::NotConnected => "not on Wi-Fi".into(),
        Wifi::Unavailable { .. } => "–".into(),
    }
}

/// `k=v, k=v` for a JSON object (strings without quotes, nested values compact).
fn kv(m: &Map<String, Value>) -> String {
    m.iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}={s}"),
            Value::Null => format!("{k}=–"),
            other => format!("{k}={other}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// [`kv`] for any value; nested objects are flattened as `outer.inner=v`.
fn kv_value(v: &Value) -> String {
    fn walk(prefix: &str, v: &Value, out: &mut Map<String, Value>) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, v, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.clone());
            }
        }
    }
    let mut flat = Map::new();
    walk("", v, &mut flat);
    kv(&flat)
}

// ------------------------------------------------------------------------------------------------ IP addresses

/// Replace every IP address in every string inside `v` with `<ip>`.
pub(crate) fn scrub(v: &mut Value) {
    match v {
        Value::String(s) => {
            let r = redact_ips(s);
            if r != *s {
                *s = r;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        Value::Object(m) => m.values_mut().for_each(scrub),
        _ => {}
    }
}

/// `s` with IPv4 (with an optional `:port`) and IPv6 addresses replaced by `<ip>`.
pub(crate) fn redact_ips(s: &str) -> String {
    redact_v6(&redact_v4(s))
}

fn redact_v4(text: &str) -> String {
    let is_part = |c: char| c.is_ascii_digit() || c == '.';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if !is_part(c) {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let run_len = rest.find(|c: char| !is_part(c)).unwrap_or(rest.len());
        let run = &rest[..run_len];
        // A trailing dot ends a sentence, not the address.
        let addr = run.trim_end_matches('.');
        if is_v4(addr) {
            out.push_str("<ip>");
            rest = &rest[addr.len()..];
            if let Some(port) = rest.strip_prefix(':') {
                let digits = port.find(|c: char| !c.is_ascii_digit()).unwrap_or(port.len());
                if digits > 0 {
                    rest = &port[digits..];
                }
            }
        } else {
            out.push_str(run);
            rest = &rest[run_len..];
        }
    }
    out
}

fn is_v4(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| (1..=3).contains(&p.len()) && p.parse::<u8>().is_ok())
}

fn redact_v6(s: &str) -> String {
    let is_part = |c: char| c.is_ascii_hexdigit() || c == ':';
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut prev: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        if is_part(c) && !prev.is_some_and(|p| p.is_alphanumeric() || p == '_' || p == ':') {
            let len = rest.find(|c: char| !is_part(c)).unwrap_or(rest.len());
            let token = &rest[..len];
            let next = rest[len..].chars().next();
            let bounded = !next.is_some_and(|n| n.is_alphanumeric() || n == '_');
            if bounded && is_v6(token) {
                out.push_str("<ip>");
            } else {
                out.push_str(token);
            }
            prev = token.chars().last();
            rest = &rest[len..];
            continue;
        }
        out.push(c);
        prev = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// A full (8 groups) or `::`-shortened IPv6 address: hex groups of 1-4 digits.
fn is_v6(t: &str) -> bool {
    let colons = t.matches(':').count();
    // No digit is needed (`fdab::abcd` is an address); a false hit like `a::b` only costs a word.
    if colons < 2 || t.contains(":::") {
        return false;
    }
    let groups: Vec<&str> = t.split(':').filter(|g| !g.is_empty()).collect();
    let short = t.contains("::");
    let ok_groups = groups.iter().all(|g| g.len() <= 4 && g.chars().all(|c| c.is_ascii_hexdigit()));
    ok_groups && ((short && groups.len() <= 7) || groups.len() == 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::WifiLink;
    use serde_json::json;

    const AT: LocalTime = LocalTime { year: 2026, month: 10, day: 26, hour: 14, minute: 3, second: 7 };

    /// A native NVENC line with the TV's STATS merged in, as the sender writes it.
    fn native_line() -> Value {
        json!({
            "arrival_gaps": 0, "arrival_ms_max": 20.4, "codec": "OMX.MTK.VIDEO.DECODER.HEVC", "decode_ms_p50": 12.7,
            "dropped": 10, "fps_in": 59.9, "fps_out": 59.9, "late": 5, "recv_kbps": 149_710,
            "enc_fps": 60.0, "send_mbps": 149.9, "cap_iv_ms": [16.7, 17.1, 18.0], "enc_ms_p50": 3.1, "enc_ms_p99": 4.0,
            "cap2tx_ms_p50": 6.2, "cap2tx_ms_p99": 8.0, "tx_ms_p99": 2.1, "tx_ms_max": 3.0, "rtt_ms": 3.4,
            "rtt_ms_last": 4.0, "e2e_ms": 13.2, "e2e_src": "tv_clock", "kf": 0, "idr_req": 0, "restarts": 0,
            "cap_ms": [0.4, 0.9], "csc_ms": [0.3, 0.5], "wait_ms": [2.0, 3.1], "nvenc_ms": [2.5, 3.3],
            "codec_out": "hevc/native", "send_size": "3840x2160@60", "target_mbps": 150.0, "fixed_rate": true,
            "note": "peer 192.0.2.20:7878 ok"
        })
    }

    fn gpu() -> GpuReading {
        GpuReading {
            name: "NVIDIA test".into(),
            source: "nvml",
            core_pct: Some(31.0),
            encoder_pct: Some(48.0),
            ..Default::default()
        }
    }

    fn pc() -> PcReading {
        PcReading {
            cpu_pct: Some(12.5),
            gpu: Some(gpu()),
            wifi: Wifi::Connected(WifiLink {
                band: Some("6 GHz".into()),
                phy: Some("802.11ax (Wi-Fi 6/6E)".into()),
                rx_mbps: Some(1201.0),
                tx_mbps: Some(1081.0),
                signal_pct: Some(92),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn a_streaming_sample_splits_the_sender_line() {
        let settings = Settings { picture: Picture::P2160x60, bitrate_mbps: Bitrate::Mbps(150), ..Default::default() };
        let s = sample(3, &State::Streaming { mode: Mode::Extend }, &settings, Some(&native_line()), pc(), None);
        assert_eq!(s.state, "streaming");
        let Tv::Stats(tv) = &s.tv else { panic!("the TV reports") };
        assert_eq!(tv["fps_out"], json!(59.9), "the TV's STATS as received");
        assert!(!tv.contains_key("enc_fps") && !tv.contains_key("rtt_ms"), "PC and network keys are not the TV's");
        assert_eq!(tv["note"], json!("peer <ip> ok"), "no IP addresses");
        assert_eq!(s.pc.stages_ms.capture, Some(0.4));
        assert_eq!(s.pc.stages_ms.convert, Some(0.3));
        assert_eq!(s.pc.stages_ms.encode, Some(2.5), "native: the NVENC time");
        assert_eq!(s.pc.stages_ms.send_p99, Some(2.1));
        assert_eq!(s.pc.encoder_fps, Some(60.0));
        assert_eq!(s.pc.sent_mbps, Some(149.9));
        assert_eq!(s.pc.cpu_pct, Some(12.5));
        assert_eq!(s.network.rtt_ms, Some(3.4));
        assert_eq!(s.network.latency_source.as_deref(), Some("tv_clock"));
        assert_eq!(s.network.tv_latency_ms, Some(5.3), "13.2 - 6.2 - 3.4 / 2");
        assert_eq!(s.network.tv_latency_source, Some(TV_LATENCY_EST));
        assert_eq!(s.network.tv_wifi, TvWifi::Missing(TV_WIFI_MISSING), "the receiver has no Wi-Fi keys yet");
        let run = s.settings.running.as_ref().expect("the sender runs");
        assert_eq!(run.path.as_deref(), Some("HEVC · NVIDIA NVENC native"));
        assert_eq!(run.auto_picked.as_deref(), Some("HEVC · NVIDIA NVENC native"), "the encoder is Auto");
        assert_eq!(s.settings.bitrate_target_mbps, Some(150));
        assert_eq!(s.settings.resolution, "3840x2160");
    }

    /// The sender's audio keys and the TV's `a_*` STATS, as PROTOCOL.md lists them.
    fn audio_line_keys() -> Value {
        let mut v = native_line();
        v.as_object_mut().unwrap().extend(
            json!({
                "audio": "on", "audio_tv_only": true, "a_sent_s": 200, "a_cap_gaps": 1, "audio_e2e_ms": 44.5,
                "av_ms": 12.0,
                "a_buf_ms": 28.0, "a_target_ms": 30.0, "a_delay_ms": 35.5, "a_out_ms": 9.0, "a_sync": "video",
                "a_mode": "exclusive", "a_pkts_s": 200, "a_underruns": 2, "a_drops": 3, "a_lost": 4, "a_skip_ms": 0,
                "a_insert_ms": 0, "a_slips": 5, "a_xruns": 0, "a_play_t_us": 1_000_000, "a_play_us": 2_000_000
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        v
    }

    #[test]
    fn audio_keys_are_carried_into_the_sample() {
        let settings = Settings { audio: AudioMode::TvOnly, ..Default::default() };
        let s = sample(3, &State::Streaming { mode: Mode::Extend }, &settings, Some(&audio_line_keys()), pc(), None);
        assert_eq!(s.settings.audio, AudioMode::TvOnly, "the run's audio setting");
        let a = s.audio.as_ref().expect("audio keys present");
        assert_eq!(a.state.as_deref(), Some("on"));
        assert_eq!(a.tv_only, Some(true));
        assert_eq!((a.e2e_ms, a.av_ms), (Some(44.5), Some(12.0)));
        assert_eq!((a.buf_ms, a.target_ms, a.delay_ms, a.out_ms), (Some(28.0), Some(30.0), Some(35.5), Some(9.0)));
        assert_eq!((a.underruns, a.drops, a.lost), (Some(2.0), Some(3.0), Some(4.0)));
        assert_eq!((a.sync.as_deref(), a.mode.as_deref()), (Some("video"), Some("exclusive")));
        assert_eq!((a.sent_per_s, a.cap_gaps, a.pkts_per_s), (Some(200.0), Some(1.0), Some(200.0)));
        let Tv::Stats(tv) = &s.tv else { panic!("the TV reports") };
        assert_eq!(tv["a_buf_ms"], json!(28.0), "the TV's audio STATS stay as received");
        let sender_keys = ["audio", "av_ms", "audio_e2e_ms", "audio_tv_only", "a_sent_s", "a_cap_gaps"];
        assert!(sender_keys.iter().all(|k| !tv.contains_key(*k)), "sender keys are not the TV's");
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["settings"]["audio"], json!("tv_only"));
        assert_eq!(j["audio"]["av_ms"], json!(12.0));
    }

    #[test]
    fn no_audio_keys_means_no_audio_block() {
        let s =
            sample(3, &State::Streaming { mode: Mode::Extend }, &Settings::default(), Some(&native_line()), pc(), None);
        assert_eq!(s.audio, None, "an old sender and TV");
        assert_eq!(s.settings.audio, AudioMode::On);
        assert_eq!(serde_json::to_value(&s).unwrap()["audio"], Value::Null);
        // The sender's audio keys alone (the TV not reporting) do not make the TV look connected.
        let line = json!({"audio": "tv_no_audio", "enc_fps": 60.0});
        let s = sample(3, &State::Streaming { mode: Mode::Extend }, &Settings::default(), Some(&line), pc(), None);
        assert_eq!(s.tv, Tv::NotConnected(NOT_CONNECTED));
        assert_eq!(s.audio.and_then(|a| a.state).as_deref(), Some("tv_no_audio"));
    }

    #[test]
    fn audio_in_the_markdown_file() {
        let settings = Settings { audio: AudioMode::Off, ..Default::default() };
        let mut r = Recording::new(PathBuf::from("x.md"), LogFormat::Markdown, AT, None);
        r.push(sample(0, &State::Streaming { mode: Mode::Extend }, &settings, Some(&audio_line_keys()), pc(), None));
        let md = r.render();
        assert!(md.contains("bitrate 40 Mb/s (the picture's default) · audio off"), "{md}");
        assert!(md.contains("Audio ms | A/V ms | Audio buf |"), "{md}");
        assert!(md.contains("| 44.5 | 12.0 | 28.0 |"), "{md}");
        assert!(md.contains("- **audio**: av_ms=12.0, buf_ms=28.0"), "{md}");
        assert!(md.contains("underruns=2.0") && md.contains("state=on"), "{md}");
    }

    #[test]
    fn audio_device_keys_are_carried_into_the_sample() {
        let mut line = audio_line_keys();
        line.as_object_mut().unwrap().extend(
            json!({"audio_dev": "Speakers (Steam Streaming Speakers)", "audio_route": "sink", "audio_pc": true})
                .as_object()
                .unwrap()
                .clone(),
        );
        let settings = Settings { audio_sink: "off".into(), ..Default::default() };
        let s = sample(3, &State::Streaming { mode: Mode::Extend }, &settings, Some(&line), pc(), None);
        let a = s.audio.as_ref().expect("audio keys present");
        assert_eq!(a.device.as_deref(), Some("Speakers (Steam Streaming Speakers)"));
        assert_eq!((a.route.as_deref(), a.pc), (Some("sink"), Some(true)));
        let Tv::Stats(tv) = &s.tv else { panic!("the TV reports") };
        assert!(
            ["audio_dev", "audio_route", "audio_pc"].iter().all(|k| !tv.contains_key(*k)),
            "sender keys, not the TV's"
        );
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["audio"]["route"], json!("sink"));
        assert_eq!(j["settings"]["audio_sink"], json!("off"));
        // The device keys alone (an idle TV) make an audio block, and do not make the TV look connected.
        let only = json!({"audio_dev": "Speakers", "audio_route": "loopback", "audio_pc": false, "enc_fps": 60.0});
        let s = sample(3, &State::Streaming { mode: Mode::Extend }, &Settings::default(), Some(&only), pc(), None);
        assert_eq!(s.tv, Tv::NotConnected(NOT_CONNECTED));
        assert_eq!(s.audio.and_then(|a| a.route).as_deref(), Some("loopback"));
    }

    /// The names `tools/runbook/add-run.ps1 -FromLog` reads: `settings.audio` and, per sample, the `audio` block.
    #[test]
    fn json_file_has_the_audio_fields_the_run_book_reads() {
        let mut r = Recording::new(PathBuf::from("x.json"), LogFormat::Json, AT, None);
        let settings = Settings { audio: AudioMode::TvOnly, ..Default::default() };
        r.push(sample(0, &State::Streaming { mode: Mode::Extend }, &settings, Some(&audio_line_keys()), pc(), None));
        r.push(sample(1, &State::Streaming { mode: Mode::Extend }, &settings, Some(&native_line()), pc(), None));
        let v: Value = serde_json::from_str(&r.render()).unwrap();
        let s0 = &v["samples"][0];
        assert_eq!(s0["settings"]["audio"], json!("tv_only"));
        for k in ["state", "tv_only", "e2e_ms", "av_ms", "buf_ms", "delay_ms", "out_ms", "underruns", "drops", "lost"] {
            assert!(!s0["audio"][k].is_null(), "samples[].audio.{k}");
        }
        assert_eq!(s0["audio"]["e2e_ms"], json!(44.5));
        // A second without audio keys (the TV stopped reporting): the block is null, the setting stays.
        assert_eq!(v["samples"][1]["audio"], Value::Null);
        assert_eq!(v["samples"][1]["settings"]["audio"], json!("tv_only"));
    }

    #[test]
    fn tv_latency_from_the_tv_wins_and_needs_an_exact_latency_otherwise() {
        let settings = Settings::default();
        let mut line = native_line();
        line["lat_ms_p50"] = json!(9.8);
        let s = sample(1, &State::Streaming { mode: Mode::Extend }, &settings, Some(&line), PcReading::default(), None);
        assert_eq!((s.network.tv_latency_ms, s.network.tv_latency_source), (Some(9.8), Some(TV_LATENCY_TV)));
        let mut line = native_line();
        line["e2e_src"] = json!("est:cap2tx+rtt/2+decode");
        let s = sample(1, &State::Streaming { mode: Mode::Extend }, &settings, Some(&line), PcReading::default(), None);
        assert_eq!((s.network.tv_latency_ms, s.network.tv_latency_source), (None, None), "not from an estimate");
    }

    #[test]
    fn a_static_second_has_no_capture_or_convert_times() {
        // The sender writes `null` for the stages it did not time (every frame repeated the last picture).
        let mut line = native_line();
        for k in ["cap_ms", "csc_ms", "wait_ms"] {
            line[k] = Value::Null;
        }
        line["new_fps"] = json!(0.0);
        let s = sample(1, &State::Streaming { mode: Mode::Extend }, &Settings::default(), Some(&line), pc(), None);
        assert_eq!((s.pc.stages_ms.capture, s.pc.stages_ms.convert), (None, None));
        assert_eq!(s.pc.stages_ms.encode, Some(2.5));
        assert_eq!(s.pc.new_fps, Some(0.0));
        assert!(!matches!(&s.tv, Tv::Stats(m) if m.contains_key("new_fps")), "new_fps is the PC's");
    }

    #[test]
    fn ffmpeg_has_no_capture_or_convert_times() {
        let line = json!({"enc_fps": 59.0, "enc_ms_p50": 5.5, "send_mbps": 40.1, "codec_out": "hevc/nvenc zero-copy"});
        let settings = Settings { encoder: Encoder::Nvenc, codec: Codec::Hevc, ..Default::default() };
        let s = sample(0, &State::Streaming { mode: Mode::Extend }, &settings, Some(&line), PcReading::default(), None);
        assert_eq!(s.pc.stages_ms.capture, None);
        assert_eq!(s.pc.stages_ms.convert, None);
        assert_eq!(s.pc.stages_ms.encode, Some(5.5));
        assert_eq!(s.tv, Tv::NotConnected(NOT_CONNECTED), "no TV keys yet");
        let run = s.settings.running.expect("running");
        assert_eq!(run.auto_picked, None, "nothing was Auto");
        assert_eq!(s.settings.bitrate_target_mbps, Some(40), "1080p60's own rate");
    }

    #[test]
    fn idle_sample_has_pc_and_settings_only() {
        let s = sample(0, &State::Idle, &Settings::default(), None, pc(), None);
        assert_eq!(s.state, "idle");
        assert_eq!(s.tv, Tv::NotConnected(NOT_CONNECTED));
        assert_eq!(s.settings.running, None);
        assert_eq!(s.pc.gpu, Some(gpu()));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["tv"], json!("not connected"));
        assert_eq!(v["pc"]["stages_ms"]["capture"], Value::Null, "missing values are null");
        assert_eq!(v["network"]["pc_wifi"]["status"], json!("connected"));
        assert_eq!(v["network"]["pc_wifi"]["band"], json!("6 GHz"));
        assert_eq!(v["settings"]["bitrate"], json!("default"));
        assert!(v.get("tv_host").is_none() && v["settings"].get("tv_host").is_none(), "no TV address");
    }

    #[test]
    fn events_for_state_changes() {
        let idle = State::Idle;
        let checking = State::Connecting { mode: Mode::Extend, phase: Phase::Checking };
        let streaming = State::Streaming { mode: Mode::Extend };
        let re = State::Connecting { mode: Mode::Extend, phase: Phase::Reconnecting };
        assert_eq!(state_event(&idle, &checking).map(|e| e.0), Some(EventKind::StreamStart));
        assert_eq!(state_event(&checking, &streaming).map(|e| e.0), Some(EventKind::Connected));
        assert_eq!(state_event(&streaming, &re).map(|e| e.0), Some(EventKind::Reconnecting));
        assert_eq!(state_event(&streaming, &State::Stopping).map(|e| e.0), Some(EventKind::StreamStop));
        assert_eq!(state_event(&State::Stopping, &idle).map(|e| e.0), Some(EventKind::Stopped));
        let err = State::Error { error: Failure::TvUnreachable { host: "192.0.2.20".into() } };
        let (kind, detail) = state_event(&checking, &err).unwrap();
        assert_eq!(kind, EventKind::Error);
        assert!(!detail.contains("192.168"), "{detail}");
        let plugging = State::Connecting { mode: Mode::Extend, phase: Phase::Plugging };
        assert_eq!(state_event(&checking, &plugging), None, "a phase is not an event");
    }

    fn recording(format: LogFormat) -> Recording {
        let mut r = Recording::new(PathBuf::from("x.log"), format, AT, Some("tunnelrez-send.exe".into()));
        r.event(0.0, EventKind::LogStart, "Log on");
        r.push(sample(0, &State::Idle, &Settings::default(), None, pc(), None));
        r.event(0.42, EventKind::StreamStart, "connecting (checking)");
        r.event(1.0, EventKind::Error, "sender: could not reach 198.51.100.7:7878");
        let settings = Settings { picture: Picture::P2160x60, ..Default::default() };
        r.push(sample(1, &State::Streaming { mode: Mode::Extend }, &settings, Some(&native_line()), pc(), None));
        r.finish(1.5, StopReason::ByHand);
        r
    }

    #[test]
    fn json_file() {
        let r = recording(LogFormat::Json);
        let v: Value = serde_json::from_str(&r.render()).unwrap();
        assert_eq!(v["layout"], json!(LAYOUT));
        assert_eq!(v["started"], json!("2026-10-26T14:03:07"));
        assert_eq!(v["seconds"], json!(2));
        assert_eq!(v["stopped"], json!("by_hand"));
        assert_eq!(v["samples"][0]["tv"], json!("not connected"));
        assert_eq!(v["samples"][1]["tv"]["decode_ms_p50"], json!(12.7));
        assert_eq!(v["events"][1]["t_s"], json!(0.4));
        assert_eq!(v["events"][3]["kind"], json!("log_stop"));
        let text = r.render();
        assert!(!text.contains("198.51.100.7") && !text.contains("192.0.2.20"), "no IP addresses");
        assert!(v["missing"].as_array().unwrap().iter().any(|m| m.as_str().unwrap().starts_with("TV Wi-Fi")));
    }

    #[test]
    fn markdown_file() {
        let r = recording(LogFormat::Markdown);
        let md = r.render();
        assert!(md.starts_with("# TunnelRez log, 2026-10-26 14:03:07"), "{md}");
        assert!(
            md.contains("Settings at the start: extend · 1080p60 · codec auto · encoder Auto · bitrate 40 Mb/s"),
            "{md}"
        );
        assert!(md.contains("| 0.4 | stream_start | connecting (checking) |"), "{md}");
        assert!(
            md.contains("| 1 | streaming | HEVC · NVIDIA NVENC native | 59.9 | 13.2 | 12.7 | 149.9 / 150 |"),
            "{md}"
        );
        assert!(md.contains("| 0 | idle | – | – |"), "idle row: TV values missing: {md}");
        assert!(md.contains("6 GHz · 802.11ax · 1201/1081 Mb/s · 92 %"), "{md}");
        assert!(md.contains("- **tv**: not connected"), "{md}");
        assert!(md.contains("- **tv**: arrival_gaps=0"), "the TV's STATS as received: {md}");
        assert!(md.contains("Missing: TV Wi-Fi"), "{md}");
        assert!(!md.contains("198.51.100.7") && !md.contains("192.0.2.20"), "no IP addresses");
    }

    #[test]
    fn finish_once() {
        let mut r = recording(LogFormat::Json);
        assert!(r.is_finished());
        r.finish(9.0, StopReason::TimeLimit);
        assert_eq!(r.events().iter().filter(|e| e.kind == EventKind::LogStop).count(), 1);
        assert_eq!(r.to_json().matches("\"by_hand\"").count(), 1);
    }

    #[test]
    fn countdown() {
        assert_eq!(remaining_s(0), 120);
        assert_eq!(remaining_s(1), 120);
        assert_eq!(remaining_s(1000), 119);
        assert_eq!(remaining_s(119_001), 1);
        assert_eq!(remaining_s(120_000), 0);
        assert_eq!(remaining_s(500_000), 0);
    }

    #[test]
    fn every_log_is_a_new_file() {
        let dir = std::env::temp_dir().join(format!("tunnelrez-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = log_path(&dir, AT, LogFormat::Json);
        assert_eq!(a.file_name().unwrap(), "2026-10-26_14-03-07.json");
        let r = Recording::new(a.clone(), LogFormat::Json, AT, None);
        r.save().unwrap();
        let b = log_path(&dir, AT, LogFormat::Json);
        assert_eq!(b.file_name().unwrap(), "2026-10-26_14-03-07_2.json");
        assert_eq!(log_path(&dir, AT, LogFormat::Markdown).file_name().unwrap(), "2026-10-26_14-03-07.md");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(saved["seconds"], json!(0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tv_wifi_when_the_tv_reports_it() {
        let line = json!({"fps_out": 60.0, "wifi_rssi_dbm": -41, "link_mbps": 866, "decode_ms_p50": 4.0});
        let s = sample(0, &State::Streaming { mode: Mode::Extend }, &Settings::default(), Some(&line), pc(), None);
        let TvWifi::Reported(w) = &s.network.tv_wifi else { panic!("reported: {:?}", s.network.tv_wifi) };
        assert_eq!(w.len(), 2);
        assert_eq!(w["wifi_rssi_dbm"], json!(-41));
        let Tv::Stats(tv) = &s.tv else { panic!("the TV reports") };
        assert!(tv.contains_key("wifi_rssi_dbm"), "still in the TV's STATS as received");
    }

    /// End to end with the real sender and `fake_tv` (no TV, no monitor plugging; it captures monitor 0): stream a
    /// few seconds, sample once per second like the controller, write both formats. By hand, with
    /// `fake_tv --listen 127.0.0.1:7979` running:
    /// `TUNNELREZ_SMOKE_SENDER=<path to tunnelrez-send.exe> cargo test smoke -- --ignored --nocapture`
    /// (`TUNNELREZ_SMOKE_PORT`, default 7979; `TUNNELREZ_SMOKE_OUT`, default the temp folder).
    #[test]
    #[ignore = "needs tunnelrez-send and a running fake_tv"]
    fn smoke_with_fake_tv() {
        use crate::platform::{local_time, PcProbe};
        use crate::stream::{Launch, Runner, StreamEvent};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let sender = PathBuf::from(std::env::var_os("TUNNELREZ_SMOKE_SENDER").expect("TUNNELREZ_SMOKE_SENDER"));
        let port = std::env::var("TUNNELREZ_SMOKE_PORT").unwrap_or_else(|_| "7979".into());
        let out = std::env::var_os("TUNNELREZ_SMOKE_OUT").map_or_else(std::env::temp_dir, PathBuf::from);
        let latest: Arc<Mutex<Option<Value>>> = Arc::default();
        let sink = Arc::clone(&latest);
        let tv_caps: Arc<Mutex<Option<crate::tvcaps::TvCaps>>> = Arc::default();
        let caps_sink = Arc::clone(&tv_caps);
        let args = ["--tv", "127.0.0.1", "--port", &port, "--monitor", "0", "--size", "1920x1080", "--fps", "60"]
            .into_iter()
            .chain(["--codec", "hevc", "--bitrate", "40M"])
            .map(String::from)
            .collect();
        let launch = Launch { sender, args, log: out.join("smoke-sender.log"), audio_restore: false };
        let runner = Runner::start(launch, move |ev| match ev {
            StreamEvent::Stats(_, v) => *sink.lock().unwrap() = Some(v),
            StreamEvent::TvCaps(c) => *caps_sink.lock().unwrap() = Some(c),
            _ => {}
        })
        .unwrap();
        let settings = Settings { codec: Codec::Hevc, ..Default::default() };
        let mut probe = PcProbe::new();
        let mut rec =
            Recording::new(out.join("smoke.json"), LogFormat::Json, local_time(), Some("tunnelrez-send.exe".into()));
        rec.event(0.0, EventKind::LogStart, "smoke test");
        for t in 0..10 {
            std::thread::sleep(Duration::from_secs(1));
            let line = latest.lock().unwrap().take();
            let state = if line.is_some() {
                State::Streaming { mode: Mode::Extend }
            } else {
                State::Connecting { mode: Mode::Extend, phase: Phase::Starting }
            };
            rec.push(sample(t, &state, &settings, line.as_ref(), probe.read(), None));
        }
        runner.stop();
        rec.finish(10.0, StopReason::ByHand);
        let json = rec.to_json();
        let md = rec.to_markdown();
        write_atomic(&out.join("smoke.json"), &json).unwrap();
        write_atomic(&out.join("smoke.md"), &md).unwrap();
        println!("wrote {} and smoke.md", out.join("smoke.json").display());
        let tv_seconds = rec.samples.iter().filter(|s| matches!(s.tv, Tv::Stats(_))).count();
        assert!(tv_seconds >= 3, "the fake TV's STATS reached the log in {tv_seconds} s");
        assert!(rec.samples.iter().any(|s| s.pc.encoder_fps.is_some_and(|f| f > 30.0)), "the sender encoded");
        assert!(!json.contains("127.0.0.1") && !md.contains("127.0.0.1"), "no IP addresses");
        // The fake TV sends CAPS like the real one: the sender passes it on and the app reads it.
        let caps = tv_caps.lock().unwrap().clone().expect("the TV's CAPS reached the app");
        assert!(caps.decodes(Codec::Av1, Picture::P2160x60), "{caps:?}");
        assert!(rec.checks().short, "10 s: a short log");
    }

    #[test]
    fn ip_addresses_are_removed() {
        assert_eq!(redact_ips("connected to 192.0.2.20:7878"), "connected to <ip>");
        assert_eq!(redact_ips("at 198.51.100.7."), "at <ip>.");
        assert_eq!(redact_ips("tv fe80::1c2b:3aff:fe4d:5e6f%12 up"), "tv <ip>%12 up");
        assert_eq!(redact_ips("[2001:db8:0:0:0:0:2:1]:7878"), "[<ip>]:7878");
        assert_eq!(redact_ips("loopback ::1"), "loopback <ip>");
        // Not addresses.
        assert_eq!(redact_ips("4.5 ms, v1.2.3, 999.1.1.1, 1.2.3.4.5"), "4.5 ms, v1.2.3, 999.1.1.1, 1.2.3.4.5");
        assert_eq!(redact_ips("at 14:03:07, std::fmt"), "at 14:03:07, std::fmt");
        // IPv6 needs no decimal digit (RFC 4291 2.2).
        assert_eq!(redact_ips("peer [fdab::abcd]:7878 and fe80::"), "peer [<ip>]:7878 and <ip>");
        assert_eq!(redact_ips("a::b"), "<ip>", "a hex word pair reads as an address: redacted, by design");
        assert_eq!(redact_ips("hevc/native 3840x2160@60"), "hevc/native 3840x2160@60");
        let mut v = json!({"a": ["203.0.113.1", {"b": "x 203.0.113.8:53"}], "n": 1});
        scrub(&mut v);
        assert_eq!(v, json!({"a": ["<ip>", {"b": "x <ip>"}], "n": 1}));
    }

    /// `n` streaming seconds of the native line, with `new_fps` and the test page per second from `f`.
    fn run_of(n: u64, f: impl Fn(u64) -> (f64, Option<&'static str>)) -> Recording {
        let mut r = Recording::new("x.json".into(), LogFormat::Json, AT, None);
        for t in 0..n {
            let (new_fps, content) = f(t);
            let mut line = native_line();
            line["new_fps"] = json!(new_fps);
            r.push(sample(
                t,
                &State::Streaming { mode: Mode::Extend },
                &Settings::default(),
                Some(&line),
                pc(),
                content,
            ));
        }
        r
    }

    #[test]
    fn checks_flag_short_logs_and_content_changes() {
        let page = Some(crate::testrun::CONTENT);
        let good = run_of(70, |_| (60.0, page)).checks();
        assert!(!good.short && !good.content_changed && !good.settings_changed, "{good:?}");
        assert_eq!(good.content.as_deref(), page, "the test page the whole time");
        assert!(good.warnings.is_empty());
        // Run 2026-10-26-13: 21 s.
        let short = run_of(21, |_| (60.0, page)).checks();
        assert!(short.short);
        assert_eq!(short.tv_seconds, 21);
        assert!(short.warnings[0].starts_with("Short log: 21 s"), "{:?}", short.warnings);
        // The page came late (opened 5 s into the log).
        let late = run_of(70, |t| (60.0, if t < 5 { None } else { page })).checks();
        assert!(late.content_changed && late.content.is_none());
        assert_eq!(late.warnings, ["The test page was not on the TV screen for the whole log."]);
        // Run 2026-10-26-11: the picture stood still for a while (the page reopening at 4K).
        let still = run_of(70, |t| (if (6..12).contains(&t) { 0.0 } else { 60.0 }, None)).checks();
        assert!(still.content_changed);
        assert_eq!(still.warnings, ["The picture stopped moving for 6 s during the log."]);
        // One slow second (run 2026-10-26-10) is not a change.
        assert!(!run_of(70, |t| (if t == 30 { 0.0 } else { 60.0 }, None)).checks().content_changed);
        // A static desktop the whole time is not a change.
        assert!(!run_of(70, |_| (0.0, None)).checks().content_changed);
        // Audio switched off half-way: a settings change (the run book keeps one stretch).
        let mut r = Recording::new("x.json".into(), LogFormat::Json, AT, None);
        for t in 0..70 {
            let audio = if t < 40 { AudioMode::On } else { AudioMode::Off };
            let settings = Settings { audio, ..Default::default() };
            r.push(sample(t, &State::Streaming { mode: Mode::Extend }, &settings, Some(&native_line()), pc(), None));
        }
        assert!(r.checks().settings_changed, "an audio change is a configuration change");
        let json: Value = serde_json::from_str(&run_of(3, |_| (60.0, page)).to_json()).unwrap();
        assert_eq!(json["checks"]["short"], json!(true));
        assert_eq!(json["checks"]["content"], json!("motion60 test page"));
        assert_eq!(json["samples"][0]["content"], json!("motion60 test page"));
    }
}
