//! The controller: owns the state machine, the settings, the sender runner and the virtual monitor, and tells the
//! panel and the tray when something changes.
//!
//! Locking: `inner` is held only for quick reads and writes, never across a blocking call. `ops` serialises the
//! slow operations (start, stop), which run on their own threads so the UI never waits on them.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::gate::{self, Blocked, Matrix};
use crate::logger::{self, EventKind, Recording, StopReason};
use crate::platform::{self, Rect, Reuse, VirtualMonitor};
use crate::runbook;
use crate::settings::{self, Codec, Encoder, LogFormat, Mode, Picture, PictureRates, Settings};
use crate::state::{self, Event, Failure, MonitorCause, Phase, State};
use crate::stream::{
    self, AudioDevices, Caps, Launch, LiveStats, Runner, SenderMonitor, SinkStatus, Source, StreamEvent, VddFix,
    VddStatus,
};
use crate::testrun::{self, TestPage, TestState, TestView};
use crate::tv::{self, Reach};
use crate::tvcaps::TvCaps;

/// Append one line to `%APPDATA%\TunnelRez\app.log`: state changes and failures, for support.
pub(crate) fn log(msg: &str) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = Settings::dir().join("app.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::create_dir_all(Settings::dir()).ok()?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    if let Some(f) = file {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let _ = writeln!(f.lock().expect("log lock poisoned"), "{t:.3} {msg}");
    }
}

/// Attempts to plug the virtual monitor before giving up.
const PLUG_TRIES: u32 = 3;
/// Seconds of latency history kept for the sparkline.
const HISTORY: usize = 60;

/// Something the app shows to the user: the panel and the tray. Implemented in `main.rs` over Tauri.
pub(crate) trait Ui: Send + Sync + 'static {
    /// The state, settings or options changed.
    fn snapshot(&self, snap: &Snapshot);
    /// New live numbers (once per second while streaming).
    fn stats(&self, stats: &LiveStats);
    /// Update the tray icon, tooltip and menu.
    fn tray(&self, state: &State, tooltip: &str);
    /// Show the panel (e.g. when a stream fails while it is hidden).
    fn show_panel(&self);
    /// The logger changed (once per second while it runs: the countdown).
    fn log_status(&self, status: &LogStatus);
    /// Show the test page as a borderless window covering `rect` (physical px): open it, or move the open one.
    ///
    /// # Errors
    /// When the window cannot be created or moved.
    fn test_page_show(&self, rect: Rect) -> anyhow::Result<()>;
    /// The test page window, when it is open.
    fn test_page(&self) -> Option<TestPage>;
    /// Close the test page (nothing when it is not open). Off the UI thread it returns only once the window is
    /// gone, so the caller may unplug the monitor next.
    fn test_page_close(&self);
    /// The caller runs on the UI (event) thread, which window calls from other threads wait for.
    fn on_ui_thread(&self) -> bool;
}

/// The logger as the panel shows it.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct LogStatus {
    /// A log is being recorded.
    pub(crate) active: bool,
    /// Seconds until it stops on its own.
    pub(crate) remaining_s: u64,
    pub(crate) max_s: u64,
    /// File name of the running log, else of the last one.
    pub(crate) file: Option<String>,
    /// Seconds recorded in that log.
    pub(crate) seconds: usize,
    /// The last log can go to the run book (it is finished, none is being recorded, and it is not in yet).
    pub(crate) runbook_ready: bool,
    pub(crate) runbook: RunbookState,
    /// Why the last write failed.
    pub(crate) error: Option<String>,
    /// The last finished log: short, or what was on screen changed (see `logger::Checks`), in words.
    pub(crate) warnings: Vec<String>,
    /// What the app knows was on screen for the whole last log (the test page), for the run book's `content`.
    pub(crate) content: Option<String>,
}

/// The "Add to run book" button.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunbookState {
    #[default]
    Idle,
    Running,
    /// The last log is in the run book (as `id`, when the importer said it): it is not added again.
    Done {
        id: Option<String>,
        message: String,
    },
    Failed {
        message: String,
    },
}

/// What the sender in use can do with sound (from its `--help`), for the panel's Audio rows.
#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioSupport {
    /// The sender has `--audio`.
    pub(crate) supported: bool,
    /// ... and `--audio tv-only` (mutes this PC's speakers while streaming).
    pub(crate) tv_only: bool,
    /// ... and `--audio-sink` (streams from a virtual playback device): the panel's Audio device picker.
    pub(crate) sink: bool,
}

impl AudioSupport {
    fn of(caps: &Caps) -> Self {
        Self { supported: caps.has_audio(), tv_only: caps.has_audio_mode("tv-only"), sink: caps.audio_sink }
    }
}

/// The panel's last "Install TV speaker".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum SinkInstall {
    #[default]
    Idle,
    Running,
    /// The install worked, but said Windows needs a restart to finish (until a status says ready).
    RebootPending,
    /// The sender's error, for the TV speaker row.
    Failed(String),
}

/// The TV speaker (the Steam Streaming Speakers driver) as the panel's TV speaker row shows it.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TvSpeaker {
    /// The sender has no `audio-sink` command.
    Unsupported,
    /// The sender could not say (its status did not run or did not parse).
    Unknown,
    Installing,
    /// The device and its playback device exist; `endpoint` is that device's name.
    Installed {
        endpoint: Option<String>,
    },
    /// Installed, but Windows must restart before it works.
    RestartRequired,
    /// The device exists, its playback device not yet (no restart needed): "Check again".
    Waiting,
    NotInstalled,
    /// The sender found no driver files: nothing to install from.
    NoDriver,
    /// The last install failed, and why.
    Failed {
        message: String,
    },
}

impl TvSpeaker {
    /// What the row shows: an install that runs, then what the status says, then the last install's error.
    fn of(caps: &Caps, status: Option<&SinkStatus>, install: &SinkInstall) -> Self {
        if !caps.tv_speaker {
            return Self::Unsupported;
        }
        if *install == SinkInstall::Running {
            return Self::Installing;
        }
        match (status, install) {
            (Some(st), _) if st.is_ready() => Self::Installed { endpoint: st.endpoint.clone() },
            (Some(st), _) if st.needs_reboot() => Self::RestartRequired,
            (_, SinkInstall::RebootPending) => Self::RestartRequired,
            (Some(st), _) if st.device => Self::Waiting,
            (Some(st), _) if st.inf.is_none() => Self::NoDriver,
            (_, SinkInstall::Failed(message)) => Self::Failed { message: message.clone() },
            (None, _) => Self::Unknown,
            (Some(_), _) => Self::NotInstalled,
        }
    }

    /// The row offers "Install TV speaker".
    fn can_install(&self) -> bool {
        matches!(self, Self::NotInstalled | Self::Failed { .. })
    }

    /// The row offers "Check again" (re-read the status).
    fn can_check(&self) -> bool {
        matches!(self, Self::Waiting)
    }
}

/// The TV speaker row: its state, and whether the Install button shows.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct TvSpeakerView {
    #[serde(flatten)]
    pub(crate) state: TvSpeaker,
    pub(crate) can_install: bool,
    pub(crate) can_check: bool,
}

impl TvSpeakerView {
    fn of(g: &Inner) -> Self {
        let state = TvSpeaker::of(&g.caps, g.sink.as_ref(), &g.sink_install);
        Self { can_install: state.can_install(), can_check: state.can_check(), state }
    }
}

/// The panel's last repair or install of the TV monitor driver.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum MonitorFix {
    #[default]
    Idle,
    Running,
    /// It worked, but Windows must restart to finish.
    RebootPending,
    /// The sender's error, and what the next click should try.
    Failed {
        message: String,
        next: VddFix,
    },
}

/// The sender's words when a repair could not move the monitor to the NVIDIA GPU (`vdd_driver::RENDER_STUCK`).
const RENDER_STUCK: &str = "the TV monitor still renders on";

/// What to try after `fix` failed with `message`: a declined prompt changed nothing, so the same again; else the
/// next bigger step (a repair that did not help ends in a reinstall).
fn after_failure(fix: VddFix, message: &str) -> VddFix {
    match fix {
        // The driver is fine: a reinstall cannot move the monitor, a restart of Windows can. Then only a restart of
        // the adapter and the check after it say anything (a plain repair finds nothing to do, and reports success).
        _ if message.contains(RENDER_STUCK) => VddFix::RepairRestart,
        _ if message.contains("declined") => fix,
        VddFix::Install => VddFix::Install,
        VddFix::Repair | VddFix::RepairRestart | VddFix::Reinstall => VddFix::Reinstall,
    }
}

/// Which GPU the sender saw the last Extend stream's monitor render on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RenderSeen {
    /// NVIDIA, or nothing streamed yet.
    #[default]
    Fine,
    /// Another GPU, on a PC where the sender wants NVIDIA.
    WrongGpu,
}

/// The TV monitor (the Parsec Virtual Display Driver) as the panel's TV monitor row shows it.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TvMonitor {
    /// The sender has no `vdd` command.
    Unsupported,
    /// The sender could not say.
    Unknown,
    Working,
    Ready {
        version: Option<String>,
    },
    NotInstalled,
    /// Not installed, and the sender found no installer.
    NoInstaller,
    RestartRequired,
    Disabled,
    AdapterProblem,
    /// The registry says the monitor renders on the integrated GPU.
    WrongGpuSetting,
    /// The registry is right, and the running stream's monitor is on the integrated GPU anyway.
    WrongGpu,
    /// A plugged monitor is stuck off the desktop.
    Stuck,
    /// The last repair or install failed, and why.
    Failed {
        message: String,
        #[serde(skip)]
        next: VddFix,
    },
}

impl TvMonitor {
    /// What the row shows: a repair that runs, then its failure, then what the status says.
    fn of(caps: &Caps, status: Option<&VddStatus>, fix: &MonitorFix, seen: RenderSeen) -> Self {
        if !caps.tv_monitor {
            return Self::Unsupported;
        }
        match fix {
            MonitorFix::Running => return Self::Working,
            MonitorFix::Failed { message, next } => return Self::Failed { message: message.clone(), next: *next },
            MonitorFix::RebootPending => return Self::RestartRequired,
            MonitorFix::Idle => {}
        }
        let Some(st) = status else { return Self::Unknown };
        match st.cause.as_str() {
            "missing" if st.installer.is_none() => Self::NoInstaller,
            "missing" => Self::NotInstalled,
            "restart_windows" => Self::RestartRequired,
            "disabled" => Self::Disabled,
            "adapter_problem" => Self::AdapterProblem,
            "render" => Self::WrongGpuSetting,
            "stuck" => Self::Stuck,
            _ if seen == RenderSeen::WrongGpu && st.render_on_nvidia.applies => Self::WrongGpu,
            _ => Self::Ready { version: st.driver_version.clone() },
        }
    }

    /// What the row's button does; `None`: no button.
    fn fix(&self) -> Option<VddFix> {
        match self {
            Self::NotInstalled => Some(VddFix::Install),
            Self::Disabled | Self::AdapterProblem | Self::WrongGpuSetting | Self::Stuck => Some(VddFix::Repair),
            // Nothing the status can name: the driver's GPU is chosen when the adapter starts.
            Self::WrongGpu => Some(VddFix::RepairRestart),
            Self::Failed { next, .. } => Some(*next),
            _ => None,
        }
    }
}

/// The TV monitor row: its state, and what its button says.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct TvMonitorView {
    #[serde(flatten)]
    pub(crate) state: TvMonitor,
    /// `repair`, `install` or `reinstall`; `None`: no button.
    pub(crate) action: Option<&'static str>,
    /// The panel's error screen can offer a repair: the sender has the `vdd` command, and nothing is running.
    pub(crate) can_fix: bool,
}

impl TvMonitorView {
    fn of(g: &Inner) -> Self {
        let state = TvMonitor::of(&g.caps, g.vdd.as_ref(), &g.monitor_fix, g.render_seen);
        let action = state.fix().map(|f| match f {
            VddFix::Repair | VddFix::RepairRestart => "repair",
            VddFix::Install => "install",
            VddFix::Reinstall => "reinstall",
        });
        let can_fix = g.caps.tv_monitor && g.monitor_fix != MonitorFix::Running;
        Self { state, action, can_fix }
    }
}

/// The fix for the error screen's Repair button: what the row would do, else a restart of the adapter (the status
/// sees nothing wrong, and the monitor still failed).
fn fix_for(g: &Inner) -> VddFix {
    let state = TvMonitor::of(&g.caps, g.vdd.as_ref(), &g.monitor_fix, g.render_seen);
    state.fix().unwrap_or(VddFix::RepairRestart)
}

/// The cause to show when the monitor failed, from what `vdd status` says now.
fn monitor_cause(status: Option<&VddStatus>) -> MonitorCause {
    match status.map(|s| s.cause.as_str()) {
        None => MonitorCause::Unknown,
        Some("missing") => MonitorCause::Missing,
        Some("restart_windows") => MonitorCause::RestartWindows,
        Some("disabled") => MonitorCause::Disabled,
        Some("adapter_problem") => MonitorCause::AdapterProblem,
        Some(_) => MonitorCause::NotAttached,
    }
}

/// Another try cannot help: only the user can change this.
fn needs_the_user(cause: MonitorCause) -> bool {
    matches!(
        cause,
        MonitorCause::Missing | MonitorCause::RestartWindows | MonitorCause::Disabled | MonitorCause::AdapterProblem
    )
}

/// The bitrate as the panel shows it: what is sent now, the hint, and the per-picture numbers (for the slider
/// while it is dragged).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct BitrateView {
    /// The fixed rate sent at the picked picture, Mb/s; `None` = Auto.
    pub(crate) effective_mbps: Option<u32>,
    /// "High bitrate at 1080p adds delay ..." when the rate is far above what the picture needs.
    pub(crate) hint: Option<String>,
    pub(crate) min: u32,
    pub(crate) max: u32,
    pub(crate) table: Vec<RateRow>,
}

/// One picture's bitrate numbers, with the hint to show above `high_mbps`.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct RateRow {
    #[serde(flatten)]
    pub(crate) rates: PictureRates,
    pub(crate) hint: Option<String>,
}

impl BitrateView {
    fn of(s: &Settings) -> Self {
        let effective = s.bitrate_mbps.effective(s.picture);
        Self {
            effective_mbps: effective,
            hint: effective.and_then(|v| settings::high_bitrate_hint(s.picture, v)),
            min: settings::BITRATE_MIN,
            max: settings::BITRATE_MAX,
            table: settings::BITRATE_TABLE
                .iter()
                .map(|r| RateRow {
                    rates: *r,
                    hint: r.high_mbps.and_then(|h| settings::high_bitrate_hint(r.picture, h + 1)),
                })
                .collect(),
        }
    }
}

/// A monitor the panel offers for Mirror.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonitorChoice {
    pub(crate) device: String,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) primary: bool,
}

/// Everything the panel renders, in one message.
#[derive(Serialize, Clone, Debug)]
pub(crate) struct Snapshot {
    pub(crate) state: State,
    pub(crate) settings: Settings,
    pub(crate) stats: Option<LiveStats>,
    /// Latency samples, ms, oldest first.
    pub(crate) history: Vec<f64>,
    /// A warning that does not stop the stream (encoder fallback).
    pub(crate) notice: Option<String>,
    pub(crate) encoders: Vec<Encoder>,
    /// Encoder x codec choices that cannot run at the picked picture, `"<encoder>|<codec>"` -> why.
    pub(crate) blocked: BTreeMap<String, Blocked>,
    /// The picked combination cannot run: Start is disabled and this shows under Advanced.
    pub(crate) block: Option<Blocked>,
    /// The sender is measuring which encoders work (first run, or a new picture).
    pub(crate) caps_checking: bool,
    pub(crate) pictures: Vec<Picture>,
    pub(crate) monitors: Vec<MonitorChoice>,
    /// File name of the sender in use.
    pub(crate) sender: Option<String>,
    pub(crate) autostart: bool,
    pub(crate) adb: bool,
    /// Windows accent palette, lightest first.
    pub(crate) accent: Option<Vec<String>>,
    pub(crate) version: &'static str,
    pub(crate) bitrate: BitrateView,
    /// Whether the sender in use has audio (the panel's Audio rows are disabled without it).
    pub(crate) audio_support: AudioSupport,
    /// The playback devices and the virtual one auto would use (`sender audio-devices`); `None` when the sender
    /// cannot list them.
    pub(crate) audio_devices: Option<AudioDevices>,
    /// The name of the virtual device auto would use; `None` = none found (or no list).
    pub(crate) audio_auto_name: Option<String>,
    /// The TV speaker row (`sender audio-sink status`, and the panel's install).
    pub(crate) tv_speaker: TvSpeakerView,
    /// The TV monitor row (`sender vdd status`, and the panel's repair).
    pub(crate) tv_monitor: TvMonitorView,
    pub(crate) log: LogStatus,
    /// The built-in test run (the Test button).
    pub(crate) test: TestView,
}

#[derive(Debug, Default)]
struct Inner {
    state: State,
    settings: Settings,
    sender: Option<PathBuf>,
    caps: Caps,
    /// The sender's playback device list, read when it is probed and again after a TV speaker install.
    audio_devices: Option<AudioDevices>,
    /// What `sender audio-sink status` said last; `None` when the sender has no such command or it failed.
    sink: Option<SinkStatus>,
    /// The panel's TV speaker install.
    sink_install: SinkInstall,
    /// What `sender vdd status` said last; `None` when the sender has no such command or it failed.
    vdd: Option<VddStatus>,
    /// The panel's repair of the TV monitor.
    monitor_fix: MonitorFix,
    /// What the sender saw the last Extend stream's monitor render on.
    render_seen: RenderSeen,
    stats: Option<LiveStats>,
    history: VecDeque<f64>,
    notice: Option<String>,
    /// Bumped on every start and stop; events from an older session are dropped.
    session: u64,
    runner: Option<Runner>,
    monitor: Option<VirtualMonitor>,
    monitors: Vec<SenderMonitor>,
    panel_visible: bool,
    /// The sender's `caps` probe per (picture, captured monitor's GDI name): results only count for the monitor
    /// they were measured on (GPU, capture path and native support depend on it).
    gate: HashMap<(Picture, String), Matrix>,
    /// A `caps` probe is running.
    caps_checking: bool,
    /// The sender's latest stats line as written, and when it came (the logger records all of it).
    line: Option<(Instant, Value)>,
    /// The codec the running stream was started with (Auto resolved) and the encoder check it used, to tell when
    /// Auto would now pick another.
    running_codec: Option<(Codec, Option<Matrix>)>,
    /// The stream is being restarted on purpose (a setting changed, or the TV's decoders became known).
    restarting: bool,
    /// The built-in test run.
    test: TestState,
}

impl Inner {
    /// The probe of `picture` on the monitor the current settings would capture, if measured.
    fn matrix(&self, picture: Picture) -> Option<&Matrix> {
        let (_, device) = self.caps_source()?;
        self.gate.get(&(picture, device))
    }

    /// Why the current settings cannot run, from the probe of their picture on their monitor. Unknown (not
    /// measured on that monitor) never blocks.
    fn block(&self) -> Option<Blocked> {
        let p = self.settings.picture;
        self.matrix(p)?.blocked(
            self.settings.encoder,
            self.settings.codec,
            p,
            self.matrix(other_picture(p)),
            self.settings.tv(),
        )
    }

    /// The monitor the current settings would capture, and its GDI name: Extend = the virtual display (None while
    /// it does not exist: a probe of another monitor says nothing about it), Mirror = the picked monitor, else the
    /// primary.
    fn caps_source(&self) -> Option<(Source, String)> {
        let want = self.settings.mirror_monitor.as_deref();
        let m = match self.settings.mode {
            Mode::Extend => self.monitors.iter().find(|m| m.is_virtual),
            Mode::Mirror => self
                .monitors
                .iter()
                .filter(|m| !m.is_virtual)
                .find(|m| want.is_some_and(|w| w == m.device))
                .or_else(|| self.monitors.iter().find(|m| m.primary))
                .or_else(|| self.monitors.first()),
        }?;
        Some((Source::Monitor(m.index), m.device.clone()))
    }
}

/// The playback devices (for the Audio device picker) and the TV speaker's status, each only when the sender has
/// the command. A failure is logged and reads as `None`.
fn read_audio(sender: &Path, caps: &Caps) -> (Option<AudioDevices>, Option<SinkStatus>) {
    fn logged<T>(what: &str, r: anyhow::Result<T>) -> Option<T> {
        r.map_err(|e| log(&format!("{what}: {e:#}"))).ok()
    }
    let devices = caps.audio_devices.then(|| logged("audio devices", stream::audio_devices(sender))).flatten();
    let sink = caps.tv_speaker.then(|| logged("TV speaker status", stream::sink_status(sender))).flatten();
    (devices, sink)
}

/// The TV monitor's status, when the sender has the command. A failure is logged and reads as `None`.
fn read_vdd(sender: &Path, caps: &Caps) -> Option<VddStatus> {
    if !caps.tv_monitor {
        return None;
    }
    stream::vdd_status(sender).map_err(|e| log(&format!("TV monitor status: {e:#}"))).ok()
}

/// The picture the panel does not have picked (to suggest it).
fn other_picture(p: Picture) -> Picture {
    match p {
        Picture::P1080x60 => Picture::P2160x60,
        Picture::P2160x60 => Picture::P1080x60,
    }
}

pub(crate) struct Core {
    ui: Box<dyn Ui>,
    inner: Mutex<Inner>,
    ops: Mutex<()>,
    /// Serialises `caps` probes: two at once would compete for the GPU/CPU and record false failures.
    caps_run: Mutex<()>,
    /// The logger. Never locked while `inner` is held, nor the other way round.
    logs: Mutex<Logs>,
    /// Tells the running test run to stop.
    test_stop: Mutex<Option<mpsc::Sender<()>>>,
    /// Serialises opening or moving the test page with `teardown`, so a page is never shown on a monitor that is
    /// being unplugged.
    page: Mutex<()>,
}

/// What a test tick read about the stream before it decided to place the page.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PageScope {
    session: u64,
    monitor: Option<String>,
}

/// The logger's state in the controller.
#[derive(Debug, Default)]
struct Logs {
    /// The log being recorded, and when it started.
    rec: Option<(Recording, Instant)>,
    /// Tells the sampler thread to stop.
    stop: Option<mpsc::Sender<StopReason>>,
    thread: Option<JoinHandle<()>>,
    /// The last finished log, kept for "Add to run book".
    last: Option<Recording>,
    runbook: RunbookState,
    error: Option<String>,
    /// What is on the TV screen now, when the app knows it (the test page fills it): written into each sample.
    content: Option<&'static str>,
}

impl Logs {
    /// The outcome of the import of the log at `source`. It only counts while that import is still the panel's
    /// (no newer log started, which resets the button): an old import never marks a newer log as added.
    fn finish_runbook(&mut self, source: &Path, outcome: RunbookState) -> bool {
        let current = self.rec.is_none()
            && self.runbook == RunbookState::Running
            && self.last.as_ref().is_some_and(|r| r.path() == source);
        if current {
            self.runbook = outcome;
        }
        current
    }

    /// "Add to run book" can run: a finished log, none recording, and that log not added (or being added) yet.
    fn runbook_open(&self) -> bool {
        self.rec.is_none()
            && self.last.is_some()
            && !matches!(self.runbook, RunbookState::Running | RunbookState::Done { .. })
    }
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core").field("inner", &self.inner).finish_non_exhaustive()
    }
}

fn reuse_policy() -> Reuse {
    if std::env::var_os("TUNNELREZ_VDD_PLUG").is_some_and(|v| v == "1") {
        Reuse::PlugOwn
    } else {
        Reuse::Existing
    }
}

impl Core {
    pub(crate) fn new(ui: impl Ui) -> Arc<Self> {
        let settings = Settings::load();
        let state = if settings.setup_done && !settings.tv_host.is_empty() { State::Idle } else { State::FirstRun };
        let sender = stream::find_sender(&settings);
        let core = Arc::new(Core {
            ui: Box::new(ui),
            inner: Mutex::new(Inner { state, settings, sender, ..Default::default() }),
            ops: Mutex::new(()),
            caps_run: Mutex::new(()),
            logs: Mutex::new(Logs::default()),
            test_stop: Mutex::new(None),
            page: Mutex::new(()),
        });
        // `--help` and `--list-monitors` take a moment; learn them off the UI thread.
        let me = Arc::clone(&core);
        std::thread::spawn(move || {
            me.probe_sender();
            me.refresh_monitors();
            // Which encoders really run: the picked picture first, then the other one (cached by the sender).
            let picture = me.lock().settings.picture;
            me.probe_caps(picture);
            me.probe_caps(other_picture(picture));
        });
        core.publish();
        core
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("core lock poisoned")
    }

    // -------------------------------------------------------------------------------------------- reads

    pub(crate) fn snapshot(&self) -> Snapshot {
        let log = self.log_status();
        let g = self.lock();
        let names = platform::display_names();
        let monitors: Vec<MonitorChoice> = g
            .monitors
            .iter()
            .filter(|m| !m.is_virtual)
            .map(|m| {
                let n = names.get(&m.device);
                let label = match n {
                    Some(n) if n.internal => "Built-in display".to_string(),
                    Some(n) if !n.friendly.is_empty() => n.friendly.clone(),
                    _ => format!("Display {}", m.index + 1),
                };
                let hz = platform::display_refresh(&m.device).map(|hz| format!(" · {hz} Hz")).unwrap_or_default();
                MonitorChoice {
                    device: m.device.clone(),
                    label,
                    detail: format!("{}×{}{hz}{}", m.width, m.height, if m.primary { " · Main" } else { "" }),
                    primary: m.primary,
                }
            })
            .collect();
        // 60 Hz only.
        let pictures = vec![Picture::P1080x60, Picture::P2160x60];
        Snapshot {
            state: g.state.clone(),
            settings: g.settings.clone(),
            stats: g.stats.clone(),
            history: g.history.iter().copied().collect(),
            notice: g.notice.clone(),
            encoders: g.caps.encoder_choices(),
            blocked: g
                .matrix(g.settings.picture)
                .map(|m| {
                    m.blocked_map(g.settings.picture, g.matrix(other_picture(g.settings.picture)), g.settings.tv())
                })
                .unwrap_or_default()
                .into_iter()
                .collect(),
            block: g.block(),
            caps_checking: g.caps_checking,
            pictures,
            monitors,
            sender: g.sender.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()),
            autostart: platform::autostart_enabled(),
            adb: tv::adb().is_some(),
            accent: platform::accent_palette(),
            version: env!("CARGO_PKG_VERSION"),
            bitrate: BitrateView::of(&g.settings),
            audio_support: AudioSupport::of(&g.caps),
            audio_devices: g.audio_devices.clone(),
            audio_auto_name: g.audio_devices.as_ref().and_then(AudioDevices::auto_name).map(str::to_string),
            tv_speaker: TvSpeakerView::of(&g),
            tv_monitor: TvMonitorView::of(&g),
            log,
            test: TestView { state: g.test.clone(), blocked: testrun::blocked(&g.state, g.settings.mode) },
        }
    }

    pub(crate) fn state(&self) -> State {
        self.lock().state.clone()
    }

    pub(crate) fn mode(&self) -> Mode {
        self.lock().settings.mode
    }

    pub(crate) fn panel_shown(&self) {
        self.lock().panel_visible = true;
    }

    pub(crate) fn panel_hidden(&self) {
        self.lock().panel_visible = false;
    }

    /// The panel stays open on blur (see [`Settings::keep_panel_open`]).
    pub(crate) fn keep_panel_open(&self) -> bool {
        self.lock().settings.keep_panel_open
    }

    /// Where the user left the kept-open panel (window top-left, physical px).
    pub(crate) fn panel_pos(&self) -> Option<(i32, i32)> {
        self.lock().settings.panel_pos
    }

    /// Remember where the kept-open panel is and save it (called after a drag settles, not per pixel).
    pub(crate) fn save_panel_pos(&self, pos: (i32, i32)) {
        let mut g = self.lock();
        if g.settings.panel_pos != Some(pos) {
            g.settings.panel_pos = Some(pos);
            if let Err(e) = g.settings.save() {
                log(&format!("save panel position: {e:#}"));
            }
        }
    }

    // -------------------------------------------------------------------------------------------- publishing

    fn tooltip(g: &Inner) -> String {
        match &g.state {
            State::Streaming { .. } => {
                let lat =
                    g.stats.as_ref().and_then(|s| s.latency_ms).map(|l| format!(", {l:.0} ms")).unwrap_or_default();
                // The size the sender really sends; the picked one only until its first stats line.
                let picture = g.stats.as_ref().and_then(|s| s.picture.as_deref()).unwrap_or(g.settings.picture.label());
                let enc = g.stats.as_ref().and_then(|s| s.encoder.as_deref()).map(settings::codec_out_words);
                let enc = match (enc, g.settings.encoder) {
                    (Some(e), Encoder::Auto) => format!(" · Auto: {e}"),
                    (Some(e), _) => format!(" · {e}"),
                    (None, _) => String::new(),
                };
                format!("TunnelRez: streaming {picture}{enc}{lat}")
            }
            State::Error { error: Failure::MonitorModeFailed { width, height } } => {
                format!("TunnelRez: could not switch the TV monitor to {}", state::size_words(*width, *height))
            }
            s => format!("TunnelRez: {}", s.summary()),
        }
    }

    fn publish(&self) {
        let snap = self.snapshot();
        let tip = Self::tooltip(&self.lock());
        self.ui.tray(&snap.state, &tip);
        self.ui.snapshot(&snap);
    }

    /// Apply a state-machine event; returns whether the state changed.
    fn apply(&self, ev: &Event) -> bool {
        let changed = {
            let mut g = self.lock();
            match g.state.on(Event::clone(ev)) {
                Some(next) if next != g.state => {
                    log(&format!("{ev:?}: {:?} -> {next:?}", g.state));
                    let logged = logger::state_event(&g.state, &next);
                    g.state = next;
                    Some(logged)
                }
                _ => None,
            }
        };
        let Some(logged) = changed else { return false };
        if let Some((kind, detail)) = logged {
            self.log_event(kind, &detail);
        }
        self.publish();
        true
    }

    // -------------------------------------------------------------------------------------------- sender info

    fn probe_sender(&self) {
        let sender = self.lock().sender.clone();
        let caps = sender.as_deref().and_then(|s| stream::probe(s).ok()).unwrap_or_default();
        let (devices, sink) = sender.as_deref().map(|s| read_audio(s, &caps)).unwrap_or_default();
        let vdd = sender.as_deref().and_then(|s| read_vdd(s, &caps));
        {
            let mut g = self.lock();
            g.caps = caps;
            g.audio_devices = devices;
            g.sink = sink;
            g.vdd = vdd;
        }
        self.publish();
    }

    /// Re-read the playback devices, the TV speaker's and the TV monitor's status (blocking: sender runs; not on the
    /// UI thread).
    pub(crate) fn refresh_audio(&self) {
        let (sender, caps) = {
            let g = self.lock();
            (g.sender.clone(), g.caps.clone())
        };
        let Some(sender) = sender else { return };
        let (devices, sink) = read_audio(&sender, &caps);
        let vdd = read_vdd(&sender, &caps);
        {
            let mut g = self.lock();
            g.audio_devices = devices;
            g.sink = sink;
            g.vdd = vdd;
        }
        self.publish();
    }

    /// The panel's Repair or Install button: `sender vdd ...` on its own thread (it shows the Windows prompt, at most
    /// one, and waits for it). Restarting the adapter takes the monitor away, so a running stream is stopped first;
    /// the stream, or the start that failed, then runs again. One at a time.
    pub(crate) fn fix_tv_monitor(self: &Arc<Self>) {
        let (sender, fix, mode, again, active) = {
            let mut g = self.lock();
            if !g.caps.tv_monitor || g.monitor_fix == MonitorFix::Running {
                return;
            }
            let Some(sender) = g.sender.clone() else { return };
            let fix = fix_for(&g);
            g.monitor_fix = MonitorFix::Running;
            let failed = matches!(
                g.state,
                State::Error { error: Failure::MonitorFailed { .. } | Failure::MonitorModeFailed { .. } }
            );
            let active = g.state.is_active();
            (sender, fix, g.settings.mode, failed || active, active)
        };
        log(&format!("TV monitor: {fix:?}"));
        self.publish();
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            // A failed start stays on screen: the Repair button is there, and any error of its own shows there too.
            if active {
                me.stop_blocking();
            }
            let (outcome, worked) = match stream::vdd_fix(&sender, fix) {
                Ok(out) => {
                    log(&format!("TV monitor: {out}"));
                    if stream::install_needs_reboot(&out) {
                        (MonitorFix::RebootPending, false)
                    } else {
                        (MonitorFix::Idle, true)
                    }
                }
                Err(e) => {
                    log(&format!("TV monitor fix: {e:#}"));
                    let message = logger::redact_ips(&format!("{e:#}"));
                    let next = after_failure(fix, &message);
                    (MonitorFix::Failed { message, next }, false)
                }
            };
            // Before the fix ends: starts are refused until then, so none can reuse an old result.
            if worked {
                me.forget_caps();
            }
            {
                let mut g = me.lock();
                g.monitor_fix = outcome;
                if worked {
                    g.render_seen = RenderSeen::Fine;
                }
            }
            me.refresh_audio();
            if worked && again {
                me.start_blocking(mode);
            }
        });
    }

    /// The panel's "Install TV speaker": `sender audio-sink install` on its own thread (it shows the Windows prompt
    /// and waits for it), then the status and the device list again, so the Audio device picker lists the new
    /// device. A running stream is left alone: the next one picks the TV speaker up. One install at a time.
    pub(crate) fn install_tv_speaker(self: &Arc<Self>) {
        let sender = {
            let mut g = self.lock();
            if !g.caps.tv_speaker || g.sink_install == SinkInstall::Running {
                return;
            }
            let Some(sender) = g.sender.clone() else { return };
            g.sink_install = SinkInstall::Running;
            sender
        };
        log("TV speaker: install");
        self.publish();
        let me = Arc::clone(self);
        let restore = self.lock().caps.tv_speaker_restore;
        std::thread::spawn(move || {
            let outcome = match stream::sink_install(&sender) {
                Ok(out) => {
                    log(&format!("TV speaker: {out}"));
                    if stream::install_needs_reboot(&out) {
                        SinkInstall::RebootPending
                    } else {
                        SinkInstall::Idle
                    }
                }
                Err(e) => {
                    log(&format!("TV speaker install: {e:#}"));
                    // A failed or killed install may have left the default playback devices switched: put back
                    // only those (a stream may be running: its mute and sink switch must stay).
                    if restore {
                        match stream::install_defaults_restore(&sender) {
                            Ok(out) => log(&format!("TV speaker: restore-defaults: {out}")),
                            Err(e) => log(&format!("TV speaker: restore-defaults: {e:#}")),
                        }
                    }
                    SinkInstall::Failed(logger::redact_ips(&format!("{e:#}")))
                }
            };
            me.lock().sink_install = outcome;
            me.refresh_audio();
        });
    }

    /// Forget every `caps` probe: a repair can move the monitor to another GPU under the same name, and the results
    /// only hold for the GPU they were measured on. Waits for a running probe, so its old result cannot land after.
    fn forget_caps(&self) {
        let _run = self.caps_run.lock().expect("caps lock poisoned");
        self.lock().gate.clear();
    }

    /// Run the sender's `caps` probe for `picture` (idle only: it encodes for a few seconds), unless it is known.
    fn probe_caps(&self, picture: Picture) {
        let _run = self.caps_run.lock().expect("caps lock poisoned");
        let (sender, source, device) = {
            let mut g = self.lock();
            // Checked again after waiting for another probe: it may have measured this, or a stream started.
            let Some((source, device)) = g.caps_source() else { return };
            if g.gate.contains_key(&(picture, device.clone())) || g.state.is_active() {
                return;
            }
            let Some(sender) = g.sender.clone() else { return };
            g.caps_checking = true;
            (sender, source, device)
        };
        self.publish();
        let result = gate::probe(&sender, picture, &source);
        {
            let mut g = self.lock();
            g.caps_checking = false;
            match result {
                Ok(m) => {
                    g.gate.insert((picture, device), m);
                }
                Err(e) => log(&format!("caps {}: {e:#}", picture.label())),
            }
        }
        self.publish();
    }

    /// Re-read the monitor list (for the Mirror picker).
    pub(crate) fn refresh_monitors(&self) {
        let Some(sender) = self.lock().sender.clone() else { return };
        if let Ok(m) = stream::list_monitors(&sender) {
            self.lock().monitors = m;
            self.publish();
        }
    }

    // -------------------------------------------------------------------------------------------- settings

    /// Change one setting from the panel. A change that affects the stream restarts it.
    pub(crate) fn set_setting(self: &Arc<Self>, key: &str, value: serde_json::Value) -> bool {
        let (ok, restart, change) = {
            let mut g = self.lock();
            let before = g.settings.clone();
            let ok = g.settings.set(key, value);
            if ok {
                let _ = g.settings.save();
            }
            let affects = match key {
                "picture" | "codec" | "encoder" | "bitrate_mbps" | "mode" | "audio" | "audio_sink" => true,
                "mirror_monitor" => g.settings.mode == Mode::Mirror,
                _ => false,
            };
            let changed = ok && before != g.settings;
            let change = changed.then(|| setting_change(key, &before, &g.settings));
            (ok, changed && affects && g.state.is_active(), change)
        };
        if let Some(c) = change {
            self.log_event(EventKind::Setting, &c);
        }
        if ok {
            if matches!(key, "picture" | "mode" | "mirror_monitor") {
                let me = Arc::clone(self);
                std::thread::spawn(move || {
                    let picture = me.lock().settings.picture;
                    me.probe_caps(picture);
                });
            }
            if key == "mode" && self.mode() == Mode::Mirror {
                let me = Arc::clone(self);
                std::thread::spawn(move || me.refresh_monitors());
            }
            self.publish();
        }
        if restart {
            self.restart_stream();
        }
        ok
    }

    /// Restart the running stream with the current settings, off the caller's thread. A test run keeps going.
    fn restart_stream(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            me.lock().restarting = true;
            me.stop_blocking();
            me.start_blocking(me.mode());
            me.lock().restarting = false;
        });
    }

    /// The first-run screen saved the TV address.
    pub(crate) fn finish_setup(&self, host: &str) {
        {
            let mut g = self.lock();
            g.settings.tv_host = host.trim().to_string();
            g.settings.setup_done = true;
            let _ = g.settings.save();
        }
        if !self.apply(&Event::SetupDone) {
            self.publish();
        }
    }

    // -------------------------------------------------------------------------------------------- start / stop

    pub(crate) fn start(self: &Arc<Self>, mode: Mode) {
        let me = Arc::clone(self);
        std::thread::spawn(move || me.start_blocking(mode));
    }

    pub(crate) fn stop(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::spawn(move || me.stop_blocking());
    }

    /// Still the same start, and nobody pressed Stop meanwhile.
    fn current(&self, session: u64) -> bool {
        let g = self.lock();
        g.session == session && matches!(g.state, State::Connecting { .. })
    }

    fn fail(&self, session: u64, failure: Failure) {
        log(&format!("fail (session {session}): {failure:?}"));
        if self.lock().session != session {
            return;
        }
        // Unplug whatever this start plugged; nothing else is running yet.
        let (runner, monitor) = {
            let mut g = self.lock();
            (g.runner.take(), g.monitor.take())
        };
        if let Some(r) = runner {
            r.stop();
        }
        drop(monitor);
        self.apply(&Event::Failed(failure));
        self.ui.show_panel();
    }

    /// Move the state machine into Connecting for a new start. `None` when the start must not go on (no TV address
    /// yet, or the state does not allow it).
    fn begin_start(&self, mode: Mode) -> Option<(Settings, u64, Option<PathBuf>, Caps)> {
        let mut g = self.lock();
        // A repair can take the monitor away under a stream. Checked here so the tray and the panel are both covered.
        if g.monitor_fix == MonitorFix::Running {
            log(&format!("start {mode:?} ignored: the TV monitor is being repaired"));
            return None;
        }
        if g.settings.tv_host.is_empty() {
            log("start without a TV address: showing the panel");
            drop(g);
            self.ui.show_panel();
            return None;
        }
        let Some(next) = g.state.on(Event::Start(mode)) else {
            log(&format!("start {mode:?} ignored in {:?}", g.state));
            return None;
        };
        log(&format!("Start: {:?} -> {next:?}", g.state));
        let logged = logger::state_event(&g.state, &next);
        g.state = next;
        g.session += 1;
        g.stats = None;
        g.line = None;
        g.history.clear();
        g.notice = None;
        g.render_seen = RenderSeen::Fine;
        if matches!(g.monitor_fix, MonitorFix::Failed { .. }) {
            g.monitor_fix = MonitorFix::Idle;
        }
        if g.settings.mode != mode {
            g.settings.mode = mode;
            let _ = g.settings.save();
        }
        let out = (g.settings.clone(), g.session, g.sender.clone(), g.caps.clone());
        drop(g);
        if let Some((kind, detail)) = logged {
            self.log_event(kind, &detail);
        }
        Some(out)
    }

    /// Extend: plug (or reuse) the virtual monitor and pick it as the capture source. On failure the start is
    /// failed and `None` comes back. What needs no administrator is tried first: a reused monitor that will not take
    /// the mode is unplugged and a fresh one plugged. The panel only hears of what the user must fix.
    fn extend_source(&self, session: u64, settings: &Settings, sender: &Path, caps: &Caps) -> Option<Source> {
        self.apply(&Event::Phase(Phase::Plugging));
        let (w, h) = settings.picture.size();
        let mut tries = 0;
        let monitor = loop {
            tries += 1;
            match VirtualMonitor::acquire(w, h, Picture::FPS, reuse_policy()) {
                Ok(m) => break m,
                Err(e) => {
                    let message = format!("{e:#}");
                    log(&format!("plug try {tries} failed: {message}"));
                    let status = read_vdd(sender, caps);
                    let cause = monitor_cause(status.as_ref());
                    self.lock().vdd = status;
                    if needs_the_user(cause) || tries >= PLUG_TRIES || !self.current(session) {
                        let failure = match stream::mode_switch_failure(&message) {
                            Some((width, height)) if !needs_the_user(cause) => {
                                Failure::MonitorModeFailed { width, height }
                            }
                            _ => Failure::MonitorFailed { cause, message },
                        };
                        self.fail(session, failure);
                        return None;
                    }
                    if !platform::virtual_display_names().is_empty() {
                        log("the TV monitor that is plugged does not work: unplugging it for a fresh one");
                        if let Err(e) = platform::unplug_virtual_displays() {
                            log(&format!("unplug: {e:#}"));
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(800));
                }
            }
        };
        let name = monitor.name().to_string();
        self.lock().monitor = Some(monitor);
        if !self.current(session) {
            return None; // stop_blocking takes and unplugs it
        }
        // `--extend` captures the first virtual display; name ours explicitly if it is not that one.
        if platform::virtual_display_names().first() == Some(&name) {
            return Some(Source::Extend);
        }
        let found = stream::list_monitors(sender).ok().and_then(|l| l.into_iter().find(|m| m.device == name));
        let Some(m) = found else {
            let message = format!("the sender cannot see the virtual monitor {name}");
            self.fail(session, Failure::MonitorFailed { cause: MonitorCause::NotAttached, message });
            return None;
        };
        Some(Source::Monitor(m.index))
    }

    /// Mirror: pick the chosen physical monitor (else the primary, else the first). On failure the start is failed
    /// and `None` comes back.
    fn mirror_source(&self, session: u64, settings: &Settings, sender: &Path) -> Option<Source> {
        let list = stream::list_monitors(sender).unwrap_or_default();
        let want = settings.mirror_monitor.as_deref();
        let pick = list
            .iter()
            .filter(|m| !m.is_virtual)
            .find(|m| want.is_some_and(|w| w == m.device))
            .or_else(|| list.iter().find(|m| m.primary))
            .or_else(|| list.first());
        let Some(m) = pick else {
            let message = "the sender found no monitor to mirror".to_string();
            self.fail(session, Failure::SenderFailed { message });
            return None;
        };
        let index = m.index;
        self.lock().monitors = list;
        Some(Source::Monitor(index))
    }

    fn start_blocking(self: &Arc<Self>, mode: Mode) {
        let _op = self.ops.lock().expect("ops lock poisoned");
        let Some((settings, session, sender, caps)) = self.begin_start(mode) else { return };
        self.publish();

        let Some(sender) = sender.or_else(|| stream::find_sender(&settings)) else {
            return self.fail(session, Failure::SenderMissing);
        };
        let caps = if caps == Caps::default() { stream::probe(&sender).unwrap_or_default() } else { caps };
        {
            let mut g = self.lock();
            g.sender = Some(sender.clone());
            g.caps = caps.clone();
        }

        let host = settings.tv_host.clone();
        match tv::check(&host) {
            Reach::Ready => {}
            Reach::AppNotOpen => return self.fail(session, Failure::TvAppNotOpen { host }),
            Reach::Unreachable | Reach::BadAddress => return self.fail(session, Failure::TvUnreachable { host }),
        }
        if !self.current(session) {
            return;
        }

        let source = match mode {
            Mode::Extend => self.extend_source(session, &settings, &sender, &caps),
            Mode::Mirror => self.mirror_source(session, &settings, &sender),
        };
        let Some(source) = source else { return };
        if !self.current(session) {
            return;
        }

        self.apply(&Event::Phase(Phase::Starting));
        // Never launch a combination the probe says cannot run; Auto codec picks one that can.
        let device = self.source_device(&sender, &source);
        let matrix = device.as_deref().and_then(|d| self.caps_for_start(&settings, &sender, &source, d));
        let other = device.and_then(|d| self.lock().gate.get(&(other_picture(settings.picture), d)).cloned());
        if let Some(b) = matrix
            .as_ref()
            .and_then(|m| m.blocked(settings.encoder, settings.codec, settings.picture, other.as_ref(), settings.tv()))
        {
            return self.fail(session, Failure::Unsupported { message: b.message, hevc_store: b.hevc_store });
        }
        // Auto codec: the best that can run on this TV (HEVC first, see `tvcaps`).
        let settings = gate::resolve_codec(&settings, matrix.as_ref());
        if !self.current(session) {
            return;
        }
        self.lock().running_codec = Some((settings.codec, matrix));
        let launch = Launch {
            args: stream::sender_args(&settings, &caps, &source),
            sender,
            log: Settings::dir().join("sender.log"),
            audio_restore: caps.audio_restore,
        };
        let weak: Weak<Core> = Arc::downgrade(self);
        let runner = Runner::start(launch, move |ev| {
            if let Some(core) = weak.upgrade() {
                core.on_stream(session, ev);
            }
        });
        match runner {
            Ok(r) => {
                let mut g = self.lock();
                if g.session == session {
                    g.runner = Some(r);
                } else {
                    drop(g);
                    r.stop();
                }
            }
            Err(e) => self.fail(session, Failure::SenderFailed { message: format!("{e:#}") }),
        }
    }

    /// The GDI name of the monitor `source` captures (`--extend` = the first virtual display).
    fn source_device(&self, sender: &Path, source: &Source) -> Option<String> {
        match source {
            Source::Extend => platform::virtual_display_names().first().cloned(),
            Source::Monitor(i) => {
                let known = self.lock().monitors.iter().find(|m| m.index == *i).map(|m| m.device.clone());
                known.or_else(|| {
                    stream::list_monitors(sender).ok()?.into_iter().find(|m| m.index == *i).map(|m| m.device)
                })
            }
        }
    }

    /// The `caps` probe for a start: the known one, else measured now on the real source (one-time, ~20 s).
    /// Waits for an idle probe still running (never two at once), then reuses its result when it measured this
    /// picture on this monitor.
    fn caps_for_start(&self, settings: &Settings, sender: &Path, source: &Source, device: &str) -> Option<Matrix> {
        let _run = self.caps_run.lock().expect("caps lock poisoned");
        let key = (settings.picture, device.to_string());
        if let Some(m) = self.lock().gate.get(&key) {
            return Some(m.clone());
        }
        match gate::probe(sender, settings.picture, source) {
            Ok(m) => {
                self.lock().gate.insert(key, m.clone());
                Some(m)
            }
            Err(e) => {
                log(&format!("caps at start: {e:#}"));
                None
            }
        }
    }

    /// Stop the stream and unplug the monitor. Blocks (up to ~3 s for the sender's clean exit).
    pub(crate) fn stop_blocking(&self) {
        let logged = {
            let mut g = self.lock();
            let Some(next) = g.state.on(Event::Stop) else { return };
            log(&format!("Stop: {:?} -> {next:?}", g.state));
            let logged = logger::state_event(&g.state, &next);
            g.state = next;
            g.session += 1;
            logged
        };
        if let Some((kind, detail)) = logged {
            self.log_event(kind, &detail);
        }
        self.publish();
        let _op = self.ops.lock().expect("ops lock poisoned");
        self.teardown();
        let changed = {
            let mut g = self.lock();
            g.stats = None;
            g.line = None;
            g.notice = None;
            g.state.on(Event::Stopped).map(|s| {
                log(&format!("Stopped: {:?} -> {s:?}", g.state));
                let logged = logger::state_event(&g.state, &s);
                g.state = s;
                logged
            })
        };
        if let Some(logged) = changed {
            if let Some((kind, detail)) = logged {
                self.log_event(kind, &detail);
            }
            self.publish();
        }
    }

    /// Close the test page, stop the sender (clean BYE) and unplug the virtual monitor, in that order: the page
    /// never outlives the TV screen (Windows would move it onto another screen).
    fn teardown(&self) {
        // Held until the monitor is gone: a test tick cannot open or move the page in between.
        let _page = self.page_lock();
        self.ui.test_page_close();
        let (runner, monitor) = {
            let mut g = self.lock();
            g.running_codec = None;
            (g.runner.take(), g.monitor.take())
        };
        if let Some(r) = runner {
            r.stop();
        }
        drop(monitor);
    }

    /// The page lock. On the UI thread only when it is free: a test tick holding it may be waiting for that very
    /// thread (window calls), so waiting there could hang the app; the page is closed anyway.
    fn page_lock(&self) -> Option<MutexGuard<'_, ()>> {
        if self.ui.on_ui_thread() {
            self.page.try_lock().ok()
        } else {
            Some(self.page.lock().expect("page lock poisoned"))
        }
    }

    /// The stream and monitor a test tick sees now.
    fn page_scope(&self) -> PageScope {
        let g = self.lock();
        PageScope { session: g.session, monitor: g.monitor.as_ref().map(|m| m.name().to_string()) }
    }

    /// Open or move the test page to `r`, unless the stream `seen` described has been stopped or restarted since
    /// (checked under the page lock, which `teardown` holds while it closes the page and unplugs the monitor).
    /// Returns whether the page was shown.
    fn place_test_page(&self, r: Rect, seen: &PageScope) -> anyhow::Result<bool> {
        let _page = self.page_lock();
        let current = {
            let g = self.lock();
            matches!(g.state, State::Streaming { .. })
                && !g.restarting
                && g.session == seen.session
                && seen.monitor.is_some()
                && g.monitor.as_ref().map(VirtualMonitor::name) == seen.monitor.as_deref()
        };
        if !current {
            return Ok(false);
        }
        self.ui.test_page_show(r)?;
        Ok(true)
    }

    /// Quit, logoff, shutdown: stop everything now, whatever the state. A running log is finished and saved.
    pub(crate) fn shutdown(&self) {
        self.stop_test();
        self.log_stop(StopReason::AppClosed);
        {
            let mut g = self.lock();
            g.session += 1;
        }
        let _op = self.ops.lock().expect("ops lock poisoned");
        self.teardown();
        self.restore_audio();
    }

    /// After the stream is gone: `sender audio-restore` puts back a default playback device or a mute that a killed
    /// sender left (nothing to do after a clean stop). Waits up to ~5 s.
    fn restore_audio(&self) {
        let sender = {
            let g = self.lock();
            g.sender.clone().filter(|_| g.caps.audio_restore)
        };
        if let Some(sender) = sender {
            match stream::audio_restore(&sender) {
                Ok(out) => log(&format!("audio-restore: {out}")),
                Err(e) => log(&format!("audio-restore: {e:#}")),
            }
        }
    }

    // -------------------------------------------------------------------------------------------- sender events

    fn on_stream(self: &Arc<Self>, session: u64, ev: StreamEvent) {
        if self.lock().session != session {
            return;
        }
        match ev {
            StreamEvent::Stats(s, line) => {
                self.lock().line = Some((Instant::now(), line));
                self.on_stats(&s);
            }
            StreamEvent::Connected => {
                self.apply(&Event::Connected);
            }
            StreamEvent::Waiting { .. } | StreamEvent::Disconnected => {
                self.lock().stats = None;
                self.apply(&Event::Lost(Phase::Reconnecting));
            }
            StreamEvent::WrongGpu => {
                let mut g = self.lock();
                if g.settings.mode == Mode::Extend {
                    g.render_seen = RenderSeen::WrongGpu;
                }
                drop(g);
                self.publish();
            }
            StreamEvent::EncoderNotice(msg) => {
                self.log_event(EventKind::Notice, &msg);
                self.lock().notice = Some(msg);
                self.publish();
            }
            StreamEvent::TvCaps(caps) => self.on_tv_caps(caps),
            StreamEvent::Restarting { message, .. } => {
                self.log_event(EventKind::Error, &format!("the sender exited: {message}"));
                self.lock().stats = None;
                self.apply(&Event::Lost(Phase::Restarting));
            }
            StreamEvent::GaveUp { message } => {
                log(&format!("sender gave up: {message}"));
                // Called on the runner's own thread: clean up from another one.
                let me = Arc::clone(self);
                std::thread::spawn(move || {
                    if me.lock().session != session {
                        return;
                    }
                    me.teardown();
                    let failure = match stream::mode_switch_failure(&message) {
                        Some((width, height)) => Failure::MonitorModeFailed { width, height },
                        None if gate::needs_hevc_extension(&message) => Failure::Unsupported {
                            message: "Windows has no HEVC encoder for this PC's graphics. Install the HEVC Video \
                                      Extensions, or pick NVENC or the CPU (they don't need it)."
                                .into(),
                            hevc_store: true,
                        },
                        None => Failure::SenderFailed { message },
                    };
                    me.apply(&Event::Failed(failure));
                    me.ui.show_panel();
                });
            }
        }
    }

    /// The TV said which decoders it has: remember them for this TV, and when Auto codec would now pick another
    /// codec than the running one (HEVC cannot run, and an AV1 decoder for 4K is first seen), restart with it.
    fn on_tv_caps(self: &Arc<Self>, caps: TvCaps) {
        let (changed, switch) = {
            let mut g = self.lock();
            let changed = g.settings.remember_tv(caps);
            if changed {
                if let Err(e) = g.settings.save() {
                    log(&format!("save TV decoders: {e:#}"));
                }
            }
            // Only new knowledge switches (a reconnect sends the same CAPS again).
            let switch = (changed && g.state.is_active() && !g.restarting)
                .then(|| auto_switch(&g.settings, g.running_codec.as_ref()))
                .flatten();
            (changed, switch.map(|c| (c, g.settings.picture)))
        };
        if changed {
            log("the TV's decoders are known");
        }
        if let Some((codec, picture)) = switch {
            let msg =
                format!("The TV decodes {} at {}: Auto codec switches to it.", codec_words(codec), picture.label());
            self.log_event(EventKind::Notice, &msg);
            self.restart_stream();
        }
    }

    fn on_stats(&self, s: &LiveStats) {
        let (visible, notice_changed) = {
            let mut g = self.lock();
            if let Some(l) = s.latency_ms.filter(|_| s.tv) {
                if g.history.len() == HISTORY {
                    g.history.pop_front();
                }
                g.history.push_back(l);
            }
            let fallback = fallback_notice(&g.settings, s.encoder.as_deref());
            let notice_changed = fallback.is_some() && g.notice.is_none();
            if notice_changed {
                g.notice = fallback;
            }
            g.stats = Some(s.clone());
            (g.panel_visible, notice_changed)
        };
        let became_streaming = self.apply(&Event::Connected);
        if notice_changed && !became_streaming {
            self.publish();
        }
        let tip = Self::tooltip(&self.lock());
        let state = self.state();
        self.ui.tray(&state, &tip);
        if visible {
            self.ui.stats(s);
        }
    }
}

impl Core {
    // -------------------------------------------------------------------------------------------- logger

    fn logs(&self) -> MutexGuard<'_, Logs> {
        self.logs.lock().expect("logger lock poisoned")
    }

    pub(crate) fn log_status(&self) -> LogStatus {
        let l = self.logs();
        let (active, remaining_s, rec) = match &l.rec {
            Some((r, t0)) => (true, logger::remaining_s(t0.elapsed().as_millis()), Some(r)),
            None => (false, 0, l.last.as_ref()),
        };
        // Checks only for a finished log (a running one is still short).
        let checks = if active { logger::Checks::default() } else { rec.map(Recording::checks).unwrap_or_default() };
        LogStatus {
            active,
            remaining_s,
            max_s: logger::MAX_SECS,
            file: rec.and_then(|r| r.path().file_name()).map(|n| n.to_string_lossy().into_owned()),
            seconds: rec.map_or(0, Recording::seconds),
            runbook_ready: l.runbook_open(),
            runbook: l.runbook.clone(),
            error: l.error.clone(),
            warnings: checks.warnings,
            content: checks.content,
        }
    }

    fn publish_log(&self) {
        let st = self.log_status();
        self.ui.log_status(&st);
    }

    /// The panel's Log switch, off: stop the log and save it.
    pub(crate) fn stop_log(&self) {
        self.log_stop(StopReason::ByHand);
    }

    /// The panel's Log switch, on: start a new log (a new file), unless one is running.
    pub(crate) fn start_log(self: &Arc<Self>) {
        let (format, sender) = {
            let g = self.lock();
            let sender = g.sender.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
            (g.settings.log_format, sender)
        };
        {
            let mut l = self.logs();
            if l.rec.is_some() {
                return;
            }
            let now = platform::local_time();
            let path = logger::log_path(&Settings::dir().join("logs"), now, format);
            let mut rec = Recording::new(path, format, now, sender);
            rec.event(0.0, EventKind::LogStart, &format!("Log on, {} s at most", logger::MAX_SECS));
            let (tx, rx) = mpsc::channel();
            let weak = Arc::downgrade(self);
            let started = Instant::now();
            // `l` is held until the log is in place, so the thread's first sample finds it.
            match std::thread::Builder::new().name("logger".into()).spawn(move || log_thread(&weak, &rx, started)) {
                Ok(thread) => {
                    l.rec = Some((rec, started));
                    l.stop = Some(tx);
                    l.thread = Some(thread);
                    l.error = None;
                    l.runbook = RunbookState::Idle;
                }
                Err(e) => l.error = Some(format!("Could not start the logger: {e}")),
            }
        }
        log("log started");
        self.publish_log();
    }

    /// Stop the running log and wait until its file is written.
    fn log_stop(&self, reason: StopReason) {
        let (tx, thread) = {
            let mut l = self.logs();
            (l.stop.take(), l.thread.take())
        };
        if let Some(tx) = tx {
            let _ = tx.send(reason);
        }
        if let Some(t) = thread {
            let _ = t.join();
        }
    }

    /// Record an event in the running log, if any.
    fn log_event(&self, kind: EventKind, detail: &str) {
        let mut l = self.logs();
        if let Some((rec, t0)) = l.rec.as_mut() {
            rec.event(t0.elapsed().as_secs_f64(), kind, detail);
        }
    }

    /// One sample, on the logger thread: read the PC first (no lock held), then the app's state.
    fn log_sample(&self, t_s: u64, probe: &mut platform::PcProbe) {
        let pc = probe.read();
        let content = self.logs().content;
        let (state, settings, line) = {
            let g = self.lock();
            let fresh = g
                .line
                .as_ref()
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(logger::STALE_SECS))
                .map(|(_, v)| v.clone());
            (g.state.clone(), g.settings.clone(), fresh)
        };
        let sample = logger::sample(t_s, &state, &settings, line.as_ref(), pc, content);
        let save = {
            let mut l = self.logs();
            let Some((rec, _)) = l.rec.as_mut() else { return };
            rec.push(sample);
            (rec.seconds() % logger::SAVE_EVERY == 0).then(|| (rec.path().to_path_buf(), rec.render()))
        };
        if let Some((path, text)) = save {
            self.log_write(&path, &text);
        }
        self.publish_log();
    }

    /// The logger thread is done: close the log, write it, keep it for the run book.
    fn log_finish(&self, reason: StopReason) {
        let done = {
            let mut l = self.logs();
            l.stop = None;
            // This very thread: dropping the handle detaches it; a waiting `log_stop` holds its own.
            l.thread = None;
            l.rec.take().map(|(mut rec, t0)| {
                rec.finish(t0.elapsed().as_secs_f64(), reason);
                let out = (rec.path().to_path_buf(), rec.render());
                l.last = Some(rec);
                out
            })
        };
        if let Some((path, text)) = done {
            self.log_write(&path, &text);
            log(&format!("log saved: {}", path.display()));
        }
        self.publish_log();
    }

    fn log_write(&self, path: &Path, text: &str) {
        if let Err(e) = logger::write_atomic(path, text) {
            log(&format!("write log: {e:#}"));
            self.logs().error = Some(format!("Could not write the log: {e:#}"));
        }
    }

    /// The panel's "Add to run book": the last log, as JSON, into `tools/runbook/add-run.ps1`, off the UI thread,
    /// with the user's `content` note. Once per log: after it is in, the button is done until the next log (the
    /// importer also skips a log it already has).
    pub(crate) fn add_to_runbook(self: &Arc<Self>, content: String) {
        let job = {
            let mut l = self.logs();
            if !l.runbook_open() {
                return;
            }
            let Some(last) = l.last.as_ref() else { return };
            // Nothing typed: what the app knows was on screen (the test page).
            let content = if content.trim().is_empty() { last.checks().content.unwrap_or_default() } else { content };
            // A JSON log is handed over as it is; a Markdown log as JSON next to it.
            let path = match last.format() {
                LogFormat::Json => last.path().to_path_buf(),
                LogFormat::Markdown => last.path().with_extension("runbook.json"),
            };
            let job = (last.path().to_path_buf(), path, last.to_json(), content);
            l.runbook = RunbookState::Running;
            job
        };
        self.publish_log();
        let sender = self.lock().sender.clone();
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let (source, path, json, content) = job;
            let result = logger::write_atomic(&path, &json).and_then(|()| {
                let script = runbook::find_script(sender.as_deref()).ok_or_else(|| {
                    anyhow::anyhow!("{} was not found next to the app or the sender", runbook::SCRIPT)
                })?;
                runbook::add(&script, &path, &content)
            });
            let outcome = match result {
                Ok(runbook::Added { id, message }) => RunbookState::Done { id, message },
                Err(e) => {
                    log(&format!("run book: {e:#}"));
                    RunbookState::Failed { message: logger::redact_ips(&format!("{e:#}")) }
                }
            };
            if me.logs().finish_runbook(&source, outcome) {
                me.publish_log();
            }
        });
    }
}

impl Core {
    // -------------------------------------------------------------------------------------------- test run

    /// The panel's Test button while no test runs: open the test page on the TV screen, then log (see `testrun`).
    pub(crate) fn start_test(self: &Arc<Self>) {
        {
            let mut g = self.lock();
            if g.test.is_on() {
                return;
            }
            if let Some(why) = testrun::blocked(&g.state, g.settings.mode) {
                g.test = TestState::Failed { message: why.into() };
                drop(g);
                self.publish();
                return;
            }
            g.test = TestState::Opening;
        }
        let (tx, rx) = mpsc::channel();
        *self.test_stop.lock().expect("test lock poisoned") = Some(tx);
        let weak = Arc::downgrade(self);
        if let Err(e) = std::thread::Builder::new().name("test-run".into()).spawn(move || test_thread(&weak, &rx)) {
            self.lock().test = TestState::Failed { message: format!("Could not start the test: {e}") };
        }
        log("test run started");
        self.publish();
    }

    /// The panel's Test button while a test runs: close the page and stop its log.
    pub(crate) fn stop_test(&self) {
        if let Some(tx) = self.test_stop.lock().expect("test lock poisoned").take() {
            let _ = tx.send(());
        }
    }

    fn set_test(&self, state: TestState) {
        let changed = {
            let mut g = self.lock();
            let changed = g.test != state;
            g.test = state;
            changed
        };
        if changed {
            self.publish();
        }
    }

    /// One look at the window: read the inputs, let `run` decide, carry it out. `Some` when the run is over.
    fn test_tick(self: &Arc<Self>, run: &mut testrun::Run) -> Option<testrun::End> {
        let (stream, mode) = {
            let g = self.lock();
            let stream = if g.restarting { testrun::Stream::Restarting } else { testrun::Stream::of(&g.state) };
            (stream, g.settings.mode)
        };
        let scope = self.page_scope();
        let monitor = scope.monitor.clone();
        let inputs = testrun::Inputs {
            stream,
            mode,
            screen: monitor.as_deref().and_then(platform::test_screen),
            page: self.ui.test_page(),
            log_running: self.logs().rec.is_some(),
            now: Instant::now(),
        };
        let tick = run.tick(&inputs);
        self.logs().content = tick.fits.then_some(testrun::CONTENT);
        match tick.action {
            testrun::Action::Nothing => {}
            testrun::Action::Close => self.ui.test_page_close(),
            testrun::Action::Place(r) => {
                // Not shown when the stream stopped or restarted after this tick read it: the next tick decides
                // again on what it sees then (a restarted stream gets the page placed anew).
                match self.place_test_page(r, &scope) {
                    Ok(true) => {}
                    Ok(false) => run.placement_skipped(),
                    Err(e) => return Some(testrun::End::Failed(format!("Could not open the test page: {e:#}"))),
                }
            }
            testrun::Action::StartLog => {
                // A fresh log for the run: one that was already recording is saved first.
                self.log_stop(StopReason::ByHand);
                self.start_log();
                let size =
                    inputs.screen.map(|r| format!("{}x{}", r.right - r.left, r.bottom - r.top)).unwrap_or_default();
                self.log_event(EventKind::Notice, &format!("Test page full screen on the TV ({size})"));
            }
        }
        self.set_test(if run.log_started() { TestState::Running } else { TestState::Opening });
        tick.end
    }

    /// The run is over: close the page, stop its log unless the log ended it, show the outcome.
    fn test_finish(&self, end: &testrun::End, run: &testrun::Run) {
        self.ui.test_page_close();
        self.logs().content = None;
        let words = match end {
            testrun::End::Stopped => "stopped",
            testrun::End::StreamStopped => "the stream stopped",
            testrun::End::LogEnded => "the log ended",
            testrun::End::Closed => "the test page was closed",
            testrun::End::Failed(m) => m.as_str(),
        };
        self.log_event(EventKind::Notice, &format!("Test run ended: {words}"));
        if run.log_started() && *end != testrun::End::LogEnded {
            self.log_stop(StopReason::TestEnded);
        }
        *self.test_stop.lock().expect("test lock poisoned") = None;
        log(&format!("test run ended: {words}"));
        self.set_test(match end {
            testrun::End::Failed(message) => TestState::Failed { message: message.clone() },
            _ => TestState::Off,
        });
    }
}

/// The test run's thread: one tick per [`testrun::TICK`] until the run ends or Test is pressed again.
fn test_thread(core: &Weak<Core>, stop: &mpsc::Receiver<()>) {
    let mut run = testrun::Run::default();
    let end = loop {
        match stop.recv_timeout(testrun::TICK) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break testrun::End::Stopped,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let Some(c) = core.upgrade() else { return };
        if let Some(e) = c.test_tick(&mut run) {
            break e;
        }
    };
    if let Some(c) = core.upgrade() {
        c.test_finish(&end, &run);
    }
}

/// The codec Auto codec would start now when it differs from the running one (`running`: the codec the stream was
/// started with and the encoder check it used). `None` for a manual codec pick, or when nothing would change.
fn auto_switch(settings: &Settings, running: Option<&(Codec, Option<Matrix>)>) -> Option<Codec> {
    let (codec, matrix) = running?;
    if settings.codec != Codec::Auto {
        return None;
    }
    let want = gate::resolve_codec(settings, matrix.as_ref()).codec;
    (want != *codec).then_some(want)
}

/// A codec in the panel's words.
fn codec_words(c: Codec) -> &'static str {
    match c {
        Codec::Auto => "Auto",
        Codec::Hevc => "HEVC",
        Codec::H264 => "H.264",
        Codec::Av1 => "AV1",
    }
}

/// The logger thread: one sample per second until it is stopped, the time limit passes, or the app closes.
fn log_thread(core: &Weak<Core>, stop: &mpsc::Receiver<StopReason>, started: Instant) {
    // Created here: it holds raw handles that stay on this thread.
    let mut probe = platform::PcProbe::new();
    let mut tick: u64 = 0;
    let reason = loop {
        match core.upgrade() {
            Some(c) => c.log_sample(tick, &mut probe),
            None => return,
        }
        tick += 1;
        let next = started + Duration::from_secs(tick);
        match stop.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(r) => break r,
            Err(RecvTimeoutError::Disconnected) => break StopReason::AppClosed,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if tick >= logger::MAX_SECS {
            break StopReason::TimeLimit;
        }
    };
    if let Some(c) = core.upgrade() {
        c.log_finish(reason);
    }
}

/// A setting change in words for the log: `picture: 1080p60 -> 4k60`. The TV address is not written.
fn setting_change(key: &str, before: &Settings, after: &Settings) -> String {
    if key == "tv_host" {
        return "tv_host: changed".into();
    }
    let field = |s: &Settings| {
        serde_json::to_value(s).ok().and_then(|v| v.get(key).cloned()).map_or_else(
            || "?".into(),
            |v| match v {
                Value::String(s) if s.is_empty() && key == "audio_sink" => "auto".into(),
                Value::String(s) => s,
                other => other.to_string(),
            },
        )
    };
    format!("{key}: {} -> {}", field(before), field(after))
}

/// Says so when the sender is not using the encoder (or codec) the user picked.
fn fallback_notice(settings: &Settings, label: Option<&str>) -> Option<String> {
    let label = label?.to_ascii_lowercase();
    let token: &[&str] = match settings.encoder {
        Encoder::Auto => &[],
        Encoder::Nvenc => &["nvenc"],
        Encoder::NvencNative => &["native"],
        Encoder::Qsv => &["qsv"],
        Encoder::Amf => &["amf"],
        Encoder::Mf => &["mf"],
        Encoder::Cpu => &["x264", "x265", "svtav1", "libx26"],
    };
    let words = settings::codec_out_words(&label);
    if !token.is_empty() && !token.iter().any(|t| label.contains(t)) {
        return Some(format!("{} did not start, so TunnelRez is using {words} instead.", settings.encoder.label()));
    }
    let codec = match settings.codec {
        crate::settings::Codec::Hevc => Some("hevc"),
        crate::settings::Codec::H264 => Some("h264"),
        crate::settings::Codec::Av1 => Some("av1"),
        crate::settings::Codec::Auto => None,
    };
    match codec {
        Some(c) if !label.starts_with(c) => {
            Some(format!("{} is not available here, so TunnelRez is using {words}.", c.to_uppercase()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Codec;

    /// A physical 1080p monitor; the first one is the primary.
    fn mon(index: usize, device: &str) -> SenderMonitor {
        SenderMonitor {
            index,
            device: device.into(),
            width: 1920,
            height: 1080,
            primary: index == 0,
            is_virtual: false,
        }
    }

    #[test]
    fn gate_results_only_count_for_the_monitor_they_were_measured_on() {
        let native_hevc_fails = Matrix::parse(
            r#"{"results":[{"encoder":"native","codec":"hevc","ok":false,"reason":"the monitor is not rendered by an NVIDIA GPU"}]}"#,
        )
        .expect("parses");
        let mut g = Inner {
            settings: Settings {
                mode: Mode::Mirror,
                encoder: Encoder::NvencNative,
                codec: Codec::Hevc,
                ..Default::default()
            },
            monitors: vec![mon(0, r"\.\DISPLAY1"), mon(1, r"\.\DISPLAY3")],
            ..Default::default()
        };
        g.gate.insert((Picture::P1080x60, r"\.\DISPLAY1".into()), native_hevc_fails);
        assert!(g.block().is_some(), "measured on the mirrored primary: blocked");
        g.settings.mirror_monitor = Some(r"\.\DISPLAY3".into());
        assert_eq!(g.block(), None, "another monitor: not measured, not blocked");
        // Extend before the virtual display exists: the primary's probe says nothing about it.
        g.settings.mode = Mode::Extend;
        assert_eq!(g.caps_source(), None);
        assert_eq!(g.block(), None);
    }

    #[test]
    fn a_repair_that_could_not_move_the_monitor_never_ends_in_a_reinstall() {
        let m = "restart the adapter, but the TV monitor still renders on Intel(R) Graphics. Restart Windows";
        for fix in [VddFix::Repair, VddFix::RepairRestart, VddFix::Install, VddFix::Reinstall] {
            assert_eq!(after_failure(fix, m), VddFix::RepairRestart, "{fix:?}");
        }
        assert_eq!(after_failure(VddFix::Repair, "it still reports stuck"), VddFix::Reinstall);
    }

    #[test]
    fn auto_codec_switches_only_when_hevc_cannot_run() {
        let tv = crate::tvcaps::TvCaps::parse(
            r#"{"decoders":[{"name":"OMX.MTK.VIDEO.DECODER.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":60,"hw":true,"perf":["3840x2160@60"]}]}"#,
        )
        .expect("caps");
        let mut s = Settings { tv_host: "tv.lan".into(), picture: Picture::P2160x60, ..Default::default() };
        assert!(s.remember_tv(tv));
        let hevc = (Codec::Hevc, None);
        assert_eq!(auto_switch(&s, Some(&hevc)), None, "4K, HEVC running, the TV has AV1: HEVC stays");
        assert_eq!(auto_switch(&s, Some(&(Codec::Av1, None))), Some(Codec::Hevc), "an AV1 stream moves to HEVC");
        assert_eq!(auto_switch(&s, None), None, "no stream");
        // HEVC cannot run here: H.264 was running, and the TV turns out to decode AV1.
        let no_hevc = Matrix::parse(
            r#"{"results":[{"encoder":"nvenc","codec":"hevc","ok":false,"reason":"no hevc encoder"},{"encoder":"nvenc","codec":"av1","ok":true,"reason":""},{"encoder":"nvenc","codec":"h264","ok":true,"reason":""}]}"#,
        );
        s.encoder = Encoder::Nvenc;
        assert_eq!(auto_switch(&s, Some(&(Codec::H264, no_hevc))), Some(Codec::Av1));
        // AV1 cannot run on the picked encoder (the check the stream started with): stay.
        let no_av1 = Matrix::parse(
            r#"{"results":[{"encoder":"qsv","codec":"av1","ok":false,"reason":"no av1 encoder"},{"encoder":"qsv","codec":"hevc","ok":true,"reason":""}]}"#,
        );
        s.encoder = Encoder::Qsv;
        assert_eq!(auto_switch(&s, Some(&(Codec::Hevc, no_av1))), None);
        s.encoder = Encoder::Auto;
        s.picture = Picture::P1080x60;
        assert_eq!(auto_switch(&s, Some(&hevc)), None, "1080p stays HEVC");
        s.picture = Picture::P2160x60;
        s.codec = Codec::Av1;
        assert_eq!(auto_switch(&s, Some(&(Codec::Av1, None))), None, "a manual pick is kept");
    }

    /// A [`Ui`] that records the test page calls; opening the page takes a while (a slow 4K window).
    #[derive(Default)]
    struct PageUi(Arc<Mutex<Vec<&'static str>>>);

    impl Ui for PageUi {
        fn snapshot(&self, _: &Snapshot) {}
        fn stats(&self, _: &LiveStats) {}
        fn tray(&self, _: &State, _: &str) {}
        fn show_panel(&self) {}
        fn log_status(&self, _: &LogStatus) {}
        fn test_page_show(&self, _: Rect) -> anyhow::Result<()> {
            self.0.lock().unwrap().push("show start");
            std::thread::sleep(Duration::from_millis(300));
            self.0.lock().unwrap().push("show end");
            Ok(())
        }
        fn test_page(&self) -> Option<TestPage> {
            None
        }
        fn test_page_close(&self) {
            self.0.lock().unwrap().push("close");
        }
        fn on_ui_thread(&self) -> bool {
            false
        }
    }

    fn core_streaming_on(monitor: &str, calls: &Arc<Mutex<Vec<&'static str>>>) -> Arc<Core> {
        let inner = Inner {
            state: State::Streaming { mode: Mode::Extend },
            session: 7,
            monitor: Some(VirtualMonitor::Borrowed { name: monitor.into() }),
            ..Default::default()
        };
        Arc::new(Core {
            ui: Box::new(PageUi(Arc::clone(calls))),
            inner: Mutex::new(inner),
            ops: Mutex::new(()),
            caps_run: Mutex::new(()),
            logs: Mutex::new(Logs::default()),
            test_stop: Mutex::new(None),
            page: Mutex::new(()),
        })
    }

    #[test]
    fn teardown_waits_for_a_page_being_opened_and_a_late_one_is_never_shown() {
        const TV: Rect = Rect { left: 2560, top: 0, right: 6400, bottom: 2160 };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let core = core_streaming_on(r"\\.\DISPLAY10", &calls);
        // A tick decided to place the page and is inside a slow window creation when Stop tears down.
        let scope = core.page_scope();
        let placing = {
            let (core, scope) = (Arc::clone(&core), scope.clone());
            std::thread::spawn(move || core.place_test_page(TV, &scope).unwrap())
        };
        std::thread::sleep(Duration::from_millis(50));
        core.lock().state = State::Stopping;
        core.teardown();
        assert!(placing.join().unwrap(), "it was already showing");
        assert_eq!(*calls.lock().unwrap(), ["show start", "show end", "close"], "closed after it, before the unplug");
        assert!(core.lock().monitor.is_none(), "the monitor is released after the close");
        // A tick that read the stream before the teardown, placing only now: nothing is shown.
        calls.lock().unwrap().clear();
        assert!(!core.place_test_page(TV, &scope).unwrap());
        assert!(calls.lock().unwrap().is_empty());
        // A restart in between (same monitor, new session) also stops a stale placement.
        let core = core_streaming_on(r"\\.\DISPLAY10", &calls);
        let scope = core.page_scope();
        core.lock().session += 2;
        assert!(!core.place_test_page(TV, &scope).unwrap());
    }

    /// The TV speaker's status: not installed, with driver files.
    fn not_installed() -> SinkStatus {
        SinkStatus { inf: Some(r"C:\drivers\SteamStreamingSpeakers.inf".into()), ..Default::default() }
    }

    /// The TV speaker's status: installed, with its playback device.
    fn installed() -> SinkStatus {
        SinkStatus { device: true, endpoint: Some("Speakers (Steam Streaming Speakers)".into()), ..not_installed() }
    }

    fn vdd(cause: &str) -> VddStatus {
        VddStatus {
            ready: cause == "none",
            cause: cause.into(),
            driver_version: Some("0.45.0.0".into()),
            installer: Some(r"C:\kit\p.exe".into()),
            render_on_nvidia: stream::VddRender { applies: true },
        }
    }

    #[test]
    fn tv_monitor_row_states() {
        let caps = Caps { tv_monitor: true, ..Default::default() };
        let of = |c: &str| TvMonitor::of(&caps, Some(&vdd(c)), &MonitorFix::Idle, RenderSeen::Fine);
        assert_eq!(
            TvMonitor::of(&Caps::default(), Some(&vdd("none")), &MonitorFix::Idle, RenderSeen::Fine),
            TvMonitor::Unsupported
        );
        assert_eq!(TvMonitor::of(&caps, None, &MonitorFix::Idle, RenderSeen::Fine), TvMonitor::Unknown);
        assert_eq!(of("none"), TvMonitor::Ready { version: Some("0.45.0.0".into()) });
        assert_eq!(of("missing"), TvMonitor::NotInstalled);
        assert_eq!(of("disabled"), TvMonitor::Disabled);
        assert_eq!(of("adapter_problem"), TvMonitor::AdapterProblem);
        assert_eq!(of("restart_windows"), TvMonitor::RestartRequired);
        assert_eq!(of("render"), TvMonitor::WrongGpuSetting);
        assert_eq!(of("stuck"), TvMonitor::Stuck);
        let none = VddStatus { installer: None, ..vdd("missing") };
        assert_eq!(TvMonitor::of(&caps, Some(&none), &MonitorFix::Idle, RenderSeen::Fine), TvMonitor::NoInstaller);
        assert_eq!(
            TvMonitor::of(&caps, Some(&vdd("none")), &MonitorFix::Running, RenderSeen::Fine),
            TvMonitor::Working
        );
        let failed = MonitorFix::Failed { message: "refused".into(), next: VddFix::Reinstall };
        assert_eq!(
            TvMonitor::of(&caps, Some(&vdd("none")), &failed, RenderSeen::Fine),
            TvMonitor::Failed { message: "refused".into(), next: VddFix::Reinstall }
        );
        assert_eq!(
            TvMonitor::of(&caps, Some(&vdd("none")), &MonitorFix::RebootPending, RenderSeen::Fine),
            TvMonitor::RestartRequired
        );
    }

    #[test]
    fn the_wrong_gpu_shows_only_where_it_can_be_fixed() {
        let caps = Caps { tv_monitor: true, ..Default::default() };
        assert_eq!(
            TvMonitor::of(&caps, Some(&vdd("none")), &MonitorFix::Idle, RenderSeen::WrongGpu),
            TvMonitor::WrongGpu
        );
        let no_nvidia = VddStatus { render_on_nvidia: stream::VddRender { applies: false }, ..vdd("none") };
        assert!(matches!(
            TvMonitor::of(&caps, Some(&no_nvidia), &MonitorFix::Idle, RenderSeen::WrongGpu),
            TvMonitor::Ready { .. }
        ));
    }

    #[test]
    fn each_button_does_the_right_fix() {
        assert_eq!(TvMonitor::NotInstalled.fix(), Some(VddFix::Install));
        assert_eq!(TvMonitor::Disabled.fix(), Some(VddFix::Repair));
        assert_eq!(TvMonitor::Stuck.fix(), Some(VddFix::Repair));
        assert_eq!(TvMonitor::WrongGpu.fix(), Some(VddFix::RepairRestart), "the status sees nothing wrong");
        assert_eq!(
            TvMonitor::Failed { message: String::new(), next: VddFix::Reinstall }.fix(),
            Some(VddFix::Reinstall)
        );
        assert_eq!(TvMonitor::Ready { version: None }.fix(), None, "no button on a healthy driver");
        assert_eq!(TvMonitor::RestartRequired.fix(), None);
        assert_eq!(TvMonitor::NoInstaller.fix(), None);
        // The error screen on a driver that looks healthy: restart it.
        let g = Inner {
            caps: Caps { tv_monitor: true, ..Default::default() },
            vdd: Some(vdd("none")),
            ..Default::default()
        };
        assert_eq!(fix_for(&g), VddFix::RepairRestart);
    }

    #[test]
    fn no_stream_starts_during_a_repair() {
        let core = core_with(Caps { tv_monitor: true, ..Default::default() }, None, SinkInstall::Idle);
        {
            let mut g = core.lock();
            g.settings.tv_host = "tv".into();
            g.state = State::Idle;
            g.monitor_fix = MonitorFix::Running;
        }
        assert!(core.begin_start(Mode::Extend).is_none());
        assert_eq!(core.state(), State::Idle, "nothing started");
        core.lock().monitor_fix = MonitorFix::Idle;
        assert!(core.begin_start(Mode::Extend).is_some());
    }

    #[test]
    fn a_repair_forgets_what_was_measured_on_the_old_gpu() {
        let core = core_with(Caps::default(), None, SinkInstall::Idle);
        core.lock().gate.insert((Picture::P1080x60, r"\\.\DISPLAY10".to_string()), Matrix::default());
        core.forget_caps();
        assert!(core.lock().gate.is_empty());
    }

    #[test]
    fn a_failed_repair_ends_in_a_reinstall_but_a_declined_prompt_does_not() {
        assert_eq!(after_failure(VddFix::Repair, "still reports adapter_problem"), VddFix::Reinstall);
        assert_eq!(after_failure(VddFix::RepairRestart, "refused"), VddFix::Reinstall);
        assert_eq!(after_failure(VddFix::Repair, "the administrator prompt was declined or failed"), VddFix::Repair);
        assert_eq!(after_failure(VddFix::Install, "the Parsec installer failed"), VddFix::Install);
    }

    #[test]
    fn only_a_missing_driver_is_called_missing() {
        assert_eq!(monitor_cause(Some(&vdd("missing"))), MonitorCause::Missing);
        assert_eq!(monitor_cause(Some(&vdd("none"))), MonitorCause::NotAttached, "the failure on this PC");
        assert_eq!(monitor_cause(Some(&vdd("stuck"))), MonitorCause::NotAttached);
        assert_eq!(monitor_cause(Some(&vdd("render"))), MonitorCause::NotAttached);
        assert_eq!(monitor_cause(None), MonitorCause::Unknown, "an older sender");
    }

    #[test]
    fn fixable_failures_are_retried_first() {
        assert!(!needs_the_user(MonitorCause::NotAttached), "unplug and plug again before saying anything");
        assert!(!needs_the_user(MonitorCause::Unknown));
        assert!(needs_the_user(MonitorCause::Missing) && needs_the_user(MonitorCause::Disabled));
        assert!(needs_the_user(MonitorCause::AdapterProblem) && needs_the_user(MonitorCause::RestartWindows));
    }

    #[test]
    fn tv_speaker_row_states() {
        let failed = SinkInstall::Failed("the Windows prompt was declined".into());
        let (not, yes, no_files) = (not_installed(), installed(), SinkStatus::default());
        let sink = Caps { tv_speaker: true, ..Default::default() };
        let of = |st: Option<&SinkStatus>, i: &SinkInstall| TvSpeaker::of(&sink, st, i);
        let old = TvSpeaker::of(&Caps::default(), Some(&yes), &SinkInstall::Idle);
        assert_eq!(old, TvSpeaker::Unsupported, "an older sender");
        assert_eq!(of(Some(&not), &SinkInstall::Idle), TvSpeaker::NotInstalled);
        assert_eq!(of(Some(&not), &SinkInstall::Running), TvSpeaker::Installing);
        assert_eq!(of(Some(&yes), &SinkInstall::Running), TvSpeaker::Installing, "the install is shown");
        let installed = TvSpeaker::Installed { endpoint: Some("Speakers (Steam Streaming Speakers)".into()) };
        assert_eq!(of(Some(&yes), &SinkInstall::Idle), installed);
        // A device without its endpoint: a restart is needed (the status or the install said so), or Windows is
        // still setting it up.
        let no_endpoint =
            SinkStatus { endpoint: None, ready: Some(false), reboot_required: Some(false), ..yes.clone() };
        let reboot = SinkStatus { reboot_required: Some(true), ..no_endpoint.clone() };
        assert_eq!(of(Some(&reboot), &SinkInstall::Idle), TvSpeaker::RestartRequired);
        assert_eq!(of(Some(&no_endpoint), &SinkInstall::Idle), TvSpeaker::Waiting);
        assert_eq!(of(Some(&no_endpoint), &SinkInstall::RebootPending), TvSpeaker::RestartRequired, "the install said");
        assert_eq!(of(None, &SinkInstall::RebootPending), TvSpeaker::RestartRequired);
        assert_eq!(of(Some(&yes), &SinkInstall::RebootPending), installed, "until a status says ready");
        let old_no_endpoint = SinkStatus { endpoint: None, ..yes.clone() };
        assert_eq!(of(Some(&old_no_endpoint), &SinkInstall::Idle), TvSpeaker::Waiting, "an older sender");
        assert_eq!(of(Some(&yes), &failed), installed, "a device that is there wins over an old error");
        assert_eq!(of(Some(&no_files), &SinkInstall::Idle), TvSpeaker::NoDriver);
        assert_eq!(of(Some(&no_files), &failed), TvSpeaker::NoDriver, "a retry cannot help");
        assert_eq!(of(Some(&not), &failed), TvSpeaker::Failed { message: "the Windows prompt was declined".into() });
        assert_eq!(of(None, &failed), TvSpeaker::Failed { message: "the Windows prompt was declined".into() });
        assert_eq!(of(None, &SinkInstall::Idle), TvSpeaker::Unknown);
        let offers: Vec<bool> = [
            TvSpeaker::Unsupported,
            TvSpeaker::Unknown,
            TvSpeaker::Installing,
            installed,
            TvSpeaker::NotInstalled,
            TvSpeaker::NoDriver,
            TvSpeaker::Failed { message: String::new() },
            TvSpeaker::RestartRequired,
            TvSpeaker::Waiting,
        ]
        .iter()
        .map(TvSpeaker::can_install)
        .collect();
        assert_eq!(
            offers,
            [false, false, false, false, true, false, true, false, false],
            "Install only when it can help"
        );
        assert!(TvSpeaker::Waiting.can_check(), "Check again while Windows sets it up");
        assert!(!TvSpeaker::RestartRequired.can_check() && !TvSpeaker::NotInstalled.can_check());
    }

    #[test]
    fn tv_speaker_view_json() {
        let g = Inner {
            caps: Caps { tv_speaker: true, ..Default::default() },
            sink: Some(not_installed()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(TvSpeakerView::of(&g)).unwrap(),
            serde_json::json!({"kind": "not_installed", "can_install": true, "can_check": false})
        );
        let g = Inner { sink: Some(installed()), ..g };
        assert_eq!(
            serde_json::to_value(TvSpeakerView::of(&g)).unwrap(),
            serde_json::json!({
                "kind": "installed", "endpoint": "Speakers (Steam Streaming Speakers)",
                "can_install": false, "can_check": false
            })
        );
        let waiting = SinkStatus { endpoint: None, ready: Some(false), reboot_required: Some(false), ..installed() };
        let w = Inner { caps: g.caps.clone(), sink: Some(waiting), ..Default::default() };
        assert_eq!(
            serde_json::to_value(TvSpeakerView::of(&w)).unwrap(),
            serde_json::json!({"kind": "waiting", "can_install": false, "can_check": true})
        );
        let g = Inner { sink: None, sink_install: SinkInstall::Failed("declined".into()), ..g };
        assert_eq!(
            serde_json::to_value(TvSpeakerView::of(&g)).unwrap(),
            serde_json::json!({"kind": "failed", "message": "declined", "can_install": true, "can_check": false})
        );
        assert_eq!(serde_json::to_value(TvSpeakerView::of(&Inner::default())).unwrap()["kind"], "unsupported");
    }

    /// A core that is idle with `caps` and `sender`; its UI records nothing.
    fn core_with(caps: Caps, sender: Option<PathBuf>, install: SinkInstall) -> Arc<Core> {
        let inner = Inner { caps, sender, sink_install: install, ..Default::default() };
        Arc::new(Core {
            ui: Box::new(PageUi::default()),
            inner: Mutex::new(inner),
            ops: Mutex::new(()),
            caps_run: Mutex::new(()),
            logs: Mutex::new(Logs::default()),
            test_stop: Mutex::new(None),
            page: Mutex::new(()),
        })
    }

    #[test]
    fn install_tv_speaker_only_starts_when_it_can() {
        // None of these runs the sender: each is refused before the install thread.
        let sink = Caps { tv_speaker: true, ..Default::default() };
        let sender = Some(PathBuf::from(r"C:\nowhere\tunnelrez-send.exe"));
        let core = core_with(Caps::default(), sender.clone(), SinkInstall::Idle);
        core.install_tv_speaker();
        assert_eq!(core.lock().sink_install, SinkInstall::Idle, "the sender has no audio-sink command");
        let core = core_with(sink.clone(), None, SinkInstall::Idle);
        core.install_tv_speaker();
        assert_eq!(core.lock().sink_install, SinkInstall::Idle, "no sender");
        let core = core_with(sink, sender, SinkInstall::Running);
        core.install_tv_speaker();
        assert_eq!(core.lock().sink_install, SinkInstall::Running, "one install at a time");
    }

    #[test]
    fn a_log_goes_into_the_run_book_once() {
        let rec = Recording::new("x.json".into(), LogFormat::Json, platform::local_time(), None);
        let mut l = Logs { last: Some(rec), ..Default::default() };
        assert!(l.runbook_open(), "a finished log");
        l.runbook = RunbookState::Running;
        assert!(!l.runbook_open(), "being added");
        l.runbook = RunbookState::Done { id: Some("2026-10-26-05".into()), message: String::new() };
        assert!(!l.runbook_open(), "added: not a second time");
        l.runbook = RunbookState::Failed { message: "no TV samples".into() };
        assert!(l.runbook_open(), "a failed add can be tried again");
        assert!(!Logs::default().runbook_open(), "no log yet");
    }

    #[test]
    fn an_old_import_does_not_mark_a_newer_log_as_added() {
        let at = platform::local_time();
        let done = || RunbookState::Done { id: Some("2026-10-26-05".into()), message: String::new() };
        let a = Path::new("a.json");
        // A's import is running; B starts (start_log resets the button), A's import ends while B records.
        let mut l = Logs { last: Some(Recording::new(a.into(), LogFormat::Json, at, None)), ..Default::default() };
        l.runbook = RunbookState::Running;
        l.rec = Some((Recording::new("b.json".into(), LogFormat::Json, at, None), Instant::now()));
        l.runbook = RunbookState::Idle;
        assert!(!l.finish_runbook(a, done()), "B is recording");
        // B finishes; A's import ends only now.
        let (b, _) = l.rec.take().expect("recording");
        l.last = Some(b);
        assert!(!l.finish_runbook(a, done()), "the last log is B");
        assert!(l.runbook_open(), "B can still go into the run book");
        // B's own import counts.
        l.runbook = RunbookState::Running;
        assert!(l.finish_runbook(Path::new("b.json"), done()));
        assert!(!l.runbook_open());
    }

    #[test]
    fn setting_changes_in_words() {
        let a = Settings { tv_host: "192.0.2.20".into(), ..Default::default() };
        let mut b = a.clone();
        b.picture = Picture::P2160x60;
        assert_eq!(setting_change("picture", &a, &b), "picture: 1080p60 -> 4k60");
        b.bitrate_mbps = settings::Bitrate::Mbps(120);
        assert_eq!(setting_change("bitrate_mbps", &a, &b), "bitrate_mbps: default -> 120");
        b.tv_host = "192.0.2.21".into();
        assert_eq!(setting_change("tv_host", &a, &b), "tv_host: changed", "no address in the log");
    }

    #[test]
    fn audio_setting_change_and_support() {
        let a = Settings::default();
        let mut b = a.clone();
        b.audio = settings::AudioMode::TvOnly;
        assert_eq!(setting_change("audio", &a, &b), "audio: on -> tv_only");
        let none = AudioSupport::of(&stream::parse_help("tunnelrez-send --tv <ip> [--encoder nvenc|qsv]"));
        assert_eq!(none, AudioSupport { supported: false, tv_only: false, sink: false }, "an older sender");
        let full = AudioSupport::of(&stream::parse_help("tunnelrez-send --tv <ip> [--audio on|off|tv-only]"));
        assert_eq!(full, AudioSupport { supported: true, tv_only: true, sink: false });
        let basic = AudioSupport::of(&stream::parse_help("tunnelrez-send --tv <ip> [--audio on|off]"));
        assert_eq!(basic, AudioSupport { supported: true, tv_only: false, sink: false });
        let sink = AudioSupport::of(&stream::parse_help(
            "tunnelrez-send --tv <ip> [--audio on|off|tv-only]\n                [--audio-sink auto|off|NAME]",
        ));
        assert_eq!(sink, AudioSupport { supported: true, tv_only: true, sink: true });
        assert_eq!(
            serde_json::to_value(sink).unwrap(),
            serde_json::json!({"supported": true, "tv_only": true, "sink": true})
        );
        let mut c = a.clone();
        c.audio_sink = "off".into();
        assert_eq!(setting_change("audio_sink", &a, &c), "audio_sink: auto -> off");
    }

    #[test]
    fn bitrate_view_hints() {
        let s = Settings { bitrate_mbps: settings::Bitrate::Mbps(120), ..Default::default() };
        let v = BitrateView::of(&s);
        assert_eq!(v.effective_mbps, Some(120));
        assert!(v.hint.as_deref().is_some_and(|h| h.starts_with("High bitrate at 1080p")), "{v:?}");
        assert_eq!(BitrateView::of(&Settings::default()).hint, None, "40 Mb/s at 1080p: fine");
        let four_k =
            Settings { picture: Picture::P2160x60, bitrate_mbps: settings::Bitrate::Mbps(150), ..Default::default() };
        assert_eq!(BitrateView::of(&four_k).hint, None);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["table"][0]["picture"], "1080p60");
        assert_eq!(json["table"][0]["default_mbps"], 40);
        assert_eq!(json["table"][0]["high_mbps"], 80);
        assert!(json["table"][0]["hint"].is_string());
        assert_eq!(json["table"][1]["hint"], Value::Null);
    }

    #[test]
    fn fallback_notices() {
        let mut s = Settings { encoder: Encoder::Nvenc, ..Default::default() };
        assert_eq!(fallback_notice(&s, Some("hevc/ffmpeg nvenc")), None);
        assert_eq!(
            fallback_notice(&s, Some("hevc/ffmpeg x265")).unwrap(),
            "NVIDIA NVENC (FFmpeg) did not start, so TunnelRez is using HEVC · CPU x265 (FFmpeg) instead."
        );
        s.encoder = Encoder::Auto;
        assert_eq!(fallback_notice(&s, Some("h264/ffmpeg qsv")), None);
        s.codec = Codec::Hevc;
        assert!(fallback_notice(&s, Some("h264/ffmpeg qsv")).unwrap().starts_with("HEVC is not available"));
        assert_eq!(fallback_notice(&s, None), None);
    }
}
