//! Runs the sender (`tunnelrez-send.exe`) as a hidden child process.
//!
//! - Learns what the sender supports from `--help`, so the app works with any sender build.
//! - Builds its command line from the settings.
//! - Parses its JSON stats (one object per line on stdout) and its `[net]` / `[encoder]` log lines (stderr).
//! - Restarts it with backoff when it dies; stops it with Ctrl+Break so it sends BYE; kills it via a job object as
//!   a last resort, and automatically if this app dies.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::platform;
use crate::settings::{Codec, Encoder, Picture, Settings};
use crate::tvcaps::TvCaps;

/// Executable names, newest first (`slipstream-send.exe` is the sender from before the rename, in an old checkout).
const SENDER_NAMES: [&str; 2] = ["tunnelrez-send.exe", "slipstream-send.exe"];
/// How long a clean stop may take (BYE, encoder shutdown) before the sender is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// A run this long counts as healthy and resets the restart backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(30);
/// Give up after this many failed runs in a row.
const MAX_FAILS: u32 = 5;
/// How long `sender audio-restore` may take before it is killed.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `sender audio-sink install` may take (it waits for the user at the Windows prompt) before it is killed.
const SINK_INSTALL_TIMEOUT: Duration = Duration::from_mins(5);
/// The sender's arguments that install the TV speaker driver (one Windows prompt; idempotent).
const SINK_INSTALL_ARGS: [&str; 2] = ["audio-sink", "install"];
/// The sender's `[warn]` line when the captured monitor renders on another GPU than NVIDIA.
const WRONG_GPU: &str = "is not on the NVIDIA GPU";

// ------------------------------------------------------------------------------------------------ discovery

/// Find the sender: the settings override, `TUNNELREZ_SENDER`, next to this app, then `sender/target/release` in a
/// source checkout above this app (development builds).
pub(crate) fn find_sender(settings: &Settings) -> Option<PathBuf> {
    if let Some(p) = settings.sender_path.as_ref().filter(|p| p.is_file()) {
        return Some(p.clone());
    }
    if let Some(p) = std::env::var_os("TUNNELREZ_SENDER").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let found = sender_candidates(exe.parent()?).find(|p| p.is_file());
    found
}

/// Where to look for the sender when the app runs from `app_dir`, best first: next to the app, then
/// `<checkout>/sender/target/release` for each folder above it (`<repo>/app/target/release` finds
/// `<repo>/sender/target/release`).
fn sender_candidates(app_dir: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let beside = SENDER_NAMES.iter().map(move |n| app_dir.join(n));
    let checkout = app_dir
        .ancestors()
        .take(5)
        .flat_map(|a| SENDER_NAMES.iter().map(move |n| a.join("sender").join("target").join("release").join(n)));
    beside.chain(checkout)
}

/// What a sender build supports, from its `--help` text.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "one independent flag per optional sender command or option")]
pub(crate) struct Caps {
    /// `--encoder` values (`nvenc`, `qsv`, `auto`, `x264`, ...).
    pub(crate) encoders: Vec<String>,
    /// `--backend` values (`ffmpeg`, `native`); empty when the flag does not exist.
    pub(crate) backends: Vec<String>,
    /// `--codec` values.
    pub(crate) codecs: Vec<String>,
    /// `--audio` values (`on`, `off`, `tv-only`); empty when the sender has no audio.
    pub(crate) audio: Vec<String>,
    /// The sender takes `--audio-sink auto|off|NAME` (streams from a virtual playback device).
    pub(crate) audio_sink: bool,
    /// The sender has the `audio-devices` command (lists the playback devices as JSON).
    pub(crate) audio_devices: bool,
    /// The sender has the `audio-restore` command (undoes what a killed sender left).
    pub(crate) audio_restore: bool,
    /// The sender has the `audio-sink` command (status, install and uninstall of the TV speaker driver).
    pub(crate) tv_speaker: bool,
    /// ... and `audio-sink restore-defaults` (puts back only what an install moved).
    pub(crate) tv_speaker_restore: bool,
    /// The sender has the `vdd` command (status, repair and install of the TV monitor driver).
    pub(crate) tv_monitor: bool,
}

impl Caps {
    /// The sender takes `--audio` at all.
    pub(crate) fn has_audio(&self) -> bool {
        !self.audio.is_empty()
    }

    /// The sender takes `--audio <mode>`.
    pub(crate) fn has_audio_mode(&self, mode: &str) -> bool {
        self.audio.iter().any(|x| x == mode)
    }

    fn has_encoder(&self, e: &str) -> bool {
        self.encoders.iter().any(|x| x == e)
    }

    /// The Encoder choices the panel should offer for this sender.
    pub(crate) fn encoder_choices(&self) -> Vec<Encoder> {
        let mut v = vec![Encoder::Auto];
        if self.has_encoder("nvenc") {
            v.push(Encoder::Nvenc);
        }
        if self.backends.iter().any(|b| b == "native") {
            v.push(Encoder::NvencNative);
        }
        if self.has_encoder("qsv") {
            v.push(Encoder::Qsv);
        }
        if self.has_encoder("amf") {
            v.push(Encoder::Amf);
        }
        if self.has_encoder("mf") {
            v.push(Encoder::Mf);
        }
        if ["x264", "x265", "svtav1"].iter().any(|e| self.has_encoder(e)) {
            v.push(Encoder::Cpu);
        }
        v
    }
}

/// The values listed for `flag` in a usage line like `[--encoder auto|nvenc|qsv]`. Placeholders (`[--codec C]`,
/// `[--size WxH|native]`) are upper case and are skipped.
fn flag_values(help: &str, flag: &str) -> Vec<String> {
    let pat = format!("[{flag} ");
    help.match_indices(&pat)
        .filter_map(|(i, _)| {
            let rest = &help[i + pat.len()..];
            rest.find(']').map(|end| &rest[..end])
        })
        .flat_map(|vals| vals.split('|'))
        .map(|v| v.trim().to_string())
        .filter(|v| {
            !v.is_empty() && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        })
        .fold(Vec::new(), |mut acc, v| {
            if !acc.contains(&v) {
                acc.push(v);
            }
            acc
        })
}

/// The `--audio` modes the sender lists: its usage line (`[--audio on|off|tv-only]`), else its option line
/// (`  --audio  on|off|tv-only ...`). Empty for a sender without audio.
fn audio_modes(help: &str) -> Vec<String> {
    let from_usage = flag_values(help, "--audio");
    if !from_usage.is_empty() {
        return from_usage;
    }
    help.lines()
        .map(str::trim_start)
        .find(|l| l.strip_prefix("--audio").is_some_and(|r| r.starts_with(char::is_whitespace)))
        .map(|l| ["on", "off", "tv-only"].iter().filter(|m| l.contains(**m)).map(|m| (*m).to_string()).collect())
        .unwrap_or_default()
}

/// The help lists `command` at the start of a line (after spaces), as `  audio-restore     put back ...` does.
fn has_command(help: &str, command: &str) -> bool {
    help.lines().any(|l| {
        l.trim_start().strip_prefix(command).is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    })
}

pub(crate) fn parse_help(help: &str) -> Caps {
    Caps {
        encoders: flag_values(help, "--encoder"),
        backends: flag_values(help, "--backend"),
        codecs: flag_values(help, "--codec"),
        audio: audio_modes(help),
        audio_sink: help.contains("[--audio-sink ") || has_command(help, "--audio-sink"),
        audio_devices: has_command(help, "audio-devices"),
        audio_restore: has_command(help, "audio-restore"),
        tv_speaker: has_command(help, "audio-sink") || help.contains("tunnelrez-send audio-sink "),
        tv_speaker_restore: help.contains("restore-defaults"),
        tv_monitor: has_command(help, "vdd"),
    }
}

/// Run `sender --help` and parse it.
///
/// # Errors
/// When the sender cannot be run.
pub(crate) fn probe(sender: &Path) -> Result<Caps> {
    let out = platform::hide_console(&mut Command::new(sender))
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} --help", sender.display()))?;
    Ok(parse_help(&String::from_utf8_lossy(&out.stdout)))
}

/// One playback device as `sender audio-devices` lists it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioDevice {
    /// The endpoint id.
    pub(crate) id: String,
    /// The friendly name, `Speakers (Steam Streaming Speakers)`; the value the panel stores for `--audio-sink`.
    pub(crate) name: String,
    /// The driver's name, `Steam Streaming Speakers`.
    #[serde(default)]
    pub(crate) adapter: String,
}

/// The playback devices, and the virtual one `--audio-sink auto` would use.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioDevices {
    #[serde(default)]
    pub(crate) devices: Vec<AudioDevice>,
    /// The endpoint id auto would pick; `None` = no virtual device found.
    #[serde(default)]
    pub(crate) auto: Option<String>,
}

impl AudioDevices {
    /// The name of the device auto would pick.
    pub(crate) fn auto_name(&self) -> Option<&str> {
        let id = self.auto.as_deref()?;
        self.devices.iter().find(|d| d.id == id).map(|d| d.name.as_str())
    }
}

/// The JSON line of `sender audio-devices` (the first line that parses; other output is skipped).
pub(crate) fn parse_audio_devices(text: &str) -> Option<AudioDevices> {
    text.lines().find_map(|l| serde_json::from_str(l.trim()).ok())
}

/// Run `sender audio-devices`.
///
/// # Errors
/// When the sender cannot be run, or prints no device list.
pub(crate) fn audio_devices(sender: &Path) -> Result<AudioDevices> {
    let out = platform::hide_console(&mut Command::new(sender))
        .arg("audio-devices")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} audio-devices", sender.display()))?;
    parse_audio_devices(&String::from_utf8_lossy(&out.stdout)).context("audio-devices printed no device list")
}

/// Run `sender audio-restore` (hidden): put back a default playback device or mute that a killed sender left.
/// Killed after [`RESTORE_TIMEOUT`]. Returns what it printed.
///
/// # Errors
/// When the sender cannot be run, fails, or takes too long.
pub(crate) fn audio_restore(sender: &Path) -> Result<String> {
    run_restore(sender, &["audio-restore"])
}

/// Run `sender audio-sink restore-defaults` (hidden): put back only the default playback devices a TV speaker
/// install moved. Unlike `audio-restore` it never touches a mute or a sink switch that a running stream owns.
///
/// # Errors
/// When the sender cannot be run, fails, or takes too long.
pub(crate) fn install_defaults_restore(sender: &Path) -> Result<String> {
    run_restore(sender, &["audio-sink", "restore-defaults"])
}

fn run_restore(sender: &Path, args: &[&str]) -> Result<String> {
    let what = args.join(" ");
    let mut child = platform::hide_console(&mut Command::new(sender))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {} {what}", sender.display()))?;
    let until = Instant::now() + RESTORE_TIMEOUT;
    let status = loop {
        if let Some(st) = child.try_wait().with_context(|| format!("wait for {what}"))? {
            break st;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{what} took longer than {} s", RESTORE_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().with_context(|| format!("read {what}"))?;
    let text =
        String::from_utf8_lossy(if out.stdout.is_empty() { &out.stderr } else { &out.stdout }).trim().to_string();
    anyhow::ensure!(status.success(), "{what} failed ({status}): {text}");
    Ok(text)
}

/// What `sender audio-sink status` says about the TV speaker (the Steam Streaming Speakers driver).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SinkStatus {
    /// The Steam Streaming Speakers root device exists.
    #[serde(default, deserialize_with = "null_false")]
    pub(crate) device: bool,
    /// Its playback endpoint's name; `None` when it has none (yet).
    #[serde(default)]
    pub(crate) endpoint: Option<String>,
    /// The driver's `.inf`; `None` when the sender found no driver files.
    #[serde(default)]
    pub(crate) inf: Option<String>,
    /// Where the driver files are: `bundled` or `steam`.
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// `TunnelRez` (or `SlipStream` before the rename) installed the device (only then may `uninstall` remove it).
    #[serde(default, alias = "installed_by_slipstream", deserialize_with = "null_false")]
    pub(crate) installed_by_tunnelrez: bool,
    /// The sender ran elevated.
    #[serde(default, deserialize_with = "null_false")]
    pub(crate) elevated: bool,
    /// The device and its playback endpoint exist; `None` from an older sender (see [`SinkStatus::is_ready`]).
    #[serde(default)]
    pub(crate) ready: Option<bool>,
    /// The install said Windows needs a restart and the endpoint is not there yet; `None` from an older sender.
    #[serde(default)]
    pub(crate) reboot_required: Option<bool>,
}

impl SinkStatus {
    /// The TV speaker can be used: the sender's `ready`, else (an older sender) a device with an endpoint.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.unwrap_or(self.device && self.endpoint.is_some())
    }

    /// Windows must restart before the TV speaker works (an older sender never says so).
    pub(crate) fn needs_reboot(&self) -> bool {
        self.reboot_required.unwrap_or(false)
    }
}

/// A JSON bool, with `null` read as `false`.
fn null_false<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(false))
}

/// The JSON line of `sender audio-sink status`: the first line that is a JSON object with a `device` key (other
/// output is skipped).
pub(crate) fn parse_sink_status(text: &str) -> Option<SinkStatus> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v.get("device").is_some())
        .and_then(|v| serde_json::from_value(v).ok())
}

/// Run `sender audio-sink status` (hidden; it changes nothing).
///
/// # Errors
/// When the sender cannot be run, or prints no status.
pub(crate) fn sink_status(sender: &Path) -> Result<SinkStatus> {
    let out = platform::hide_console(&mut Command::new(sender))
        .args(["audio-sink", "status"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} audio-sink status", sender.display()))?;
    parse_sink_status(&String::from_utf8_lossy(&out.stdout)).context("audio-sink status printed no status")
}

/// The hidden command that installs the TV speaker driver: `sender audio-sink install`.
fn sink_install_command(sender: &Path) -> Command {
    let mut cmd = Command::new(sender);
    platform::hide_console(&mut cmd).args(SINK_INSTALL_ARGS);
    cmd
}

/// Install the TV speaker driver: `sender audio-sink install`, hidden. The sender shows the one Windows prompt
/// itself and waits for the elevated install, so this blocks for seconds (longer while the prompt is open); never
/// call it on the UI thread. Killed after [`SINK_INSTALL_TIMEOUT`]. Returns what it printed.
///
/// # Errors
/// When the sender cannot be run, takes too long, or fails (the prompt was declined, the driver did not install):
/// the error is the sender's `Error: ...` line.
pub(crate) fn sink_install(sender: &Path) -> Result<String> {
    run_driver_fix(sink_install_command(sender))
}

/// What `sender vdd status` says about the TV monitor (the Parsec Virtual Display Driver).
#[derive(Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct VddStatus {
    /// Nothing is wrong.
    #[serde(default, deserialize_with = "null_false")]
    pub(crate) ready: bool,
    /// The first thing wrong: `none`, `missing`, `restart_windows`, `disabled`, `adapter_problem`, `render`, `stuck`.
    #[serde(default)]
    pub(crate) cause: String,
    /// The installed version.
    #[serde(default)]
    pub(crate) driver_version: Option<String>,
    /// The bundled installer; `None` when the sender found none.
    #[serde(default)]
    pub(crate) installer: Option<String>,
    #[serde(default)]
    pub(crate) render_on_nvidia: VddRender,
}

/// The `render_on_nvidia` part of [`VddStatus`].
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VddRender {
    /// This PC has an NVIDIA GPU to render on.
    #[serde(default, deserialize_with = "null_false")]
    pub(crate) applies: bool,
}

/// The JSON line of `sender vdd status`: the first line that is a JSON object with a `cause` key.
pub(crate) fn parse_vdd_status(text: &str) -> Option<VddStatus> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v.get("cause").is_some())
        .and_then(|v| serde_json::from_value(v).ok())
}

/// Run `sender vdd status` (hidden; it changes nothing).
///
/// # Errors
/// When the sender cannot be run, or prints no status.
pub(crate) fn vdd_status(sender: &Path) -> Result<VddStatus> {
    let out = platform::hide_console(&mut Command::new(sender))
        .args(["vdd", "status"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} vdd status", sender.display()))?;
    parse_vdd_status(&String::from_utf8_lossy(&out.stdout)).context("vdd status printed no status")
}

/// What the panel's button asks of the TV monitor driver.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VddFix {
    /// Fix only what `vdd status` reports.
    #[default]
    Repair,
    /// The same, and restart the adapter although nothing looks wrong (the monitor still fails).
    RepairRestart,
    /// Run the bundled installer.
    Install,
    /// Run it over a driver that looks fine and does not work.
    Reinstall,
}

impl VddFix {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Repair => &["vdd", "repair"],
            Self::RepairRestart => &["vdd", "repair", "--restart-adapter"],
            Self::Install => &["vdd", "install"],
            Self::Reinstall => &["vdd", "install", "--force"],
        }
    }
}

/// Repair or install the TV monitor driver, hidden: like [`sink_install`], one Windows prompt at most, and blocking.
/// Returns what the sender printed.
///
/// # Errors
/// When the sender cannot be run, takes too long, or fails: the error is the sender's `Error: ...` line.
pub(crate) fn vdd_fix(sender: &Path, fix: VddFix) -> Result<String> {
    let mut cmd = Command::new(sender);
    platform::hide_console(&mut cmd).args(fix.args());
    run_driver_fix(cmd)
}

/// Run a driver command that may wait at the Windows prompt. Killed after [`SINK_INSTALL_TIMEOUT`].
fn run_driver_fix(mut cmd: Command) -> Result<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", cmd.get_program().to_string_lossy()))?;
    // The output is one line, far below a pipe's buffer: polling cannot block the sender on a full pipe.
    let until = Instant::now() + SINK_INSTALL_TIMEOUT;
    while child.try_wait().context("wait for the driver command")?.is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the install took longer than {} s", SINK_INSTALL_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output().context("read the driver command")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        return Ok(stdout);
    }
    anyhow::bail!("{}", sink_install_error(&String::from_utf8_lossy(&out.stderr), &stdout, out.status.code()))
}

/// A successful install's output says Windows needs a restart to finish it.
pub(crate) fn install_needs_reboot(output: &str) -> bool {
    output.contains("REBOOT_REQUIRED")
}

/// The words for a failed install: the sender's last `Error: ...` line (without the prefix), else its last line
/// of output, else the exit code.
fn sink_install_error(stderr: &str, stdout: &str, code: Option<i32>) -> String {
    let last = |t: &str| t.lines().map(str::trim).rfind(|l| !l.is_empty()).map(str::to_string);
    let error = stderr.lines().filter_map(|l| l.trim().strip_prefix("Error:")).map(str::trim).rfind(|l| !l.is_empty());
    error.map(str::to_string).or_else(|| last(stderr)).or_else(|| last(stdout)).unwrap_or_else(|| match code {
        Some(c) => format!("the install failed (exit code {c})"),
        None => "the install was stopped".to_string(),
    })
}

/// One capturable monitor as `--list-monitors` prints it.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SenderMonitor {
    /// The value for `--monitor`.
    pub(crate) index: usize,
    /// GDI name, `\\.\DISPLAY1`.
    pub(crate) device: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) primary: bool,
    /// A Parsec virtual display.
    pub(crate) is_virtual: bool,
}

/// Parse `  [0] \\.\DISPLAY1     2560x1600 at (0,0)  primary` lines; other lines are skipped.
pub(crate) fn parse_monitors(text: &str) -> Vec<SenderMonitor> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let rest = line.strip_prefix('[')?;
            let (idx, rest) = rest.split_once(']')?;
            let mut tok = rest.split_whitespace();
            let device = tok.next()?.to_string();
            let (w, h) = tok.next()?.split_once('x')?;
            Some(SenderMonitor {
                index: idx.trim().parse().ok()?,
                device,
                width: w.parse().ok()?,
                height: h.parse().ok()?,
                primary: line.contains("  primary"),
                is_virtual: line.contains("virtual("),
            })
        })
        .collect()
}

/// Run `sender --list-monitors`.
///
/// # Errors
/// When the sender cannot be run.
pub(crate) fn list_monitors(sender: &Path) -> Result<Vec<SenderMonitor>> {
    let out = platform::hide_console(&mut Command::new(sender))
        .arg("--list-monitors")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} --list-monitors", sender.display()))?;
    Ok(parse_monitors(&String::from_utf8_lossy(&out.stdout)))
}

// ------------------------------------------------------------------------------------------------ command line

/// Which screen the sender captures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The (only) Parsec virtual display: `--extend`.
    Extend,
    /// A monitor by `--list-monitors` index.
    Monitor(usize),
}

/// The sender's command line for these settings. Options the sender does not support are left out.
pub(crate) fn sender_args(settings: &Settings, caps: &Caps, source: &Source) -> Vec<String> {
    let mut a: Vec<String> = vec!["--tv".into(), settings.tv_host.clone()];
    match source {
        Source::Extend => a.push("--extend".into()),
        Source::Monitor(i) => a.extend(["--monitor".into(), i.to_string()]),
    }
    let (w, h) = settings.picture.size();
    a.extend(["--size".into(), format!("{w}x{h}"), "--fps".into(), Picture::FPS.to_string()]);
    let has_codec = |c: &str| caps.codecs.is_empty() || caps.codecs.iter().any(|x| x == c);
    let codec = match settings.codec {
        Codec::Auto => has_codec("hevc").then_some("hevc"),
        Codec::Hevc => Some("hevc"),
        Codec::H264 => Some("h264"),
        Codec::Av1 => Some("av1"),
    };
    if let Some(c) = codec {
        a.extend(["--codec".into(), c.into()]);
    }
    let encoder = match settings.encoder {
        Encoder::Auto => None,
        Encoder::Nvenc => Some("nvenc"),
        Encoder::Qsv => Some("qsv"),
        Encoder::Amf => Some("amf"),
        Encoder::Mf => Some("mf"),
        Encoder::Cpu => match codec.unwrap_or("h264") {
            "hevc" => Some("x265"),
            "av1" => Some("svtav1"),
            _ => Some("x264"),
        },
        Encoder::NvencNative => {
            a.extend(["--backend".into(), "native".into()]);
            None
        }
    };
    if let Some(e) = encoder.filter(|e| caps.has_encoder(e)) {
        a.extend(["--encoder".into(), e.into()]);
    }
    // A set bitrate is a fixed rate: the sender turns padding on for an explicit --bitrate. Not picked: the
    // picture's own rate; Auto: the sender's ceiling.
    if let Some(b) = settings.bitrate_mbps.effective(settings.picture) {
        a.extend(["--bitrate".into(), format!("{b}M")]);
    }
    // On is the sender's default: nothing is passed. Off and tv-only only for a sender that knows them.
    if let Some(m) = settings.audio.flag_value().filter(|m| caps.has_audio_mode(m)) {
        a.extend(["--audio".into(), m.into()]);
    }
    // Auto is the sender's default: nothing is passed. Off or a device only for a sender that knows the flag.
    if let Some(v) = settings.audio_sink_flag().filter(|_| caps.audio_sink) {
        a.extend(["--audio-sink".into(), v.into()]);
    }
    a
}

// ------------------------------------------------------------------------------------------------ stats

/// The target bitrate's meaning, as the panel shows it.
#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RateState {
    /// The target is a ceiling (Auto, or an older sender): sending less is normal.
    #[default]
    Ceiling,
    /// A fixed rate, being met.
    Fixed,
    /// A fixed rate under [`RATE_LOW_SHARE`] of its target for [`RATE_LOW_SECS`] stats lines in a row.
    Low,
}

/// The numbers the panel shows, from one sender stats line.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct LiveStats {
    /// Frames per second shown on the TV (encoded fps if the TV has not reported yet).
    pub(crate) fps: Option<f64>,
    /// Capture to display, ms.
    pub(crate) latency_ms: Option<f64>,
    /// `latency_ms` comes from the TV's display clock (exact), not an estimate.
    pub(crate) latency_exact: bool,
    /// TV decode time, median, ms.
    pub(crate) decode_ms: Option<f64>,
    /// Megabits per second on the wire.
    pub(crate) mbps: Option<f64>,
    /// Megabits per second the sender really sent in the last second.
    pub(crate) sent_mbps: Option<f64>,
    /// The encoder's target, Mb/s.
    pub(crate) target_mbps: Option<f64>,
    /// What the target means, and whether a fixed rate is being missed.
    pub(crate) rate: RateState,
    /// Frames the TV dropped in the last second.
    pub(crate) dropped: Option<f64>,
    /// Round trip to the TV, ms.
    pub(crate) rtt_ms: Option<f64>,
    /// What the sender is really using, e.g. `hevc/ffmpeg nvenc`.
    pub(crate) encoder: Option<String>,
    /// The size really encoded and sent, as the sender says it (`3840x2160@60`).
    pub(crate) size: Option<String>,
    /// `size` in the panel's words (`4K60`, `1080p60`, `1440p120`).
    pub(crate) picture: Option<String>,
    /// The TV's stats were fresh in this line.
    pub(crate) tv: bool,
    /// The panel's audio line (`Audio: on · 45 ms · A/V +12 ms · buf 28 ms`); `None` when there is nothing to say
    /// (no `audio` key: an old sender, or audio off).
    pub(crate) audio_line: Option<String>,
    /// `Audio device: Speakers (Steam Streaming Speakers) · also on PC`; `None` until the sender says which device
    /// it captures (`audio_dev`).
    pub(crate) audio_device_line: Option<String>,
    /// How the sound is captured: `sink` (a virtual device) or `loopback` (the PC speakers); `None` when not said.
    pub(crate) audio_route: Option<String>,
}

/// Milliseconds as a whole number: `45 ms`.
fn ms_words(v: f64) -> String {
    format!("{:.0} ms", v.max(0.0))
}

/// A signed offset: `+12 ms`, `-3 ms`, `0 ms`.
fn signed_ms_words(v: f64) -> String {
    let r = v.round();
    if r == 0.0 {
        "0 ms".into()
    } else {
        format!("{r:+.0} ms")
    }
}

/// The audio line of the panel from one sender stats line. `audio` is `off | waiting | tv_no_audio | on | error`;
/// `off` and a missing key say nothing. `av_ms` is positive when the sound is behind the picture.
pub(crate) fn audio_line(v: &Value) -> Option<String> {
    let f = |k: &str| v.get(k).and_then(Value::as_f64);
    match v.get("audio").and_then(Value::as_str)? {
        "tv_no_audio" => Some("Audio: TV has no audio support".into()),
        "waiting" => Some("Audio: waiting for the TV".into()),
        "error" => Some("Audio: error".into()),
        "on" => {
            let mut parts = vec!["Audio: on".to_string()];
            if v.get("audio_tv_only").and_then(Value::as_bool) == Some(true) {
                parts.push("TV only".into());
            }
            if let Some(e2e) = f("audio_e2e_ms") {
                parts.push(ms_words(e2e));
            }
            if let Some(av) = f("av_ms") {
                parts.push(format!("A/V {}", signed_ms_words(av)));
            }
            if let Some(buf) = f("a_buf_ms") {
                parts.push(format!("buf {}", ms_words(buf)));
            }
            Some(parts.join(" · "))
        }
        _ => None,
    }
}

/// The device line of the panel from the sender's `audio_dev`, `audio_route` and `audio_pc` keys.
pub(crate) fn audio_device_line(v: &Value) -> Option<String> {
    let dev = v.get("audio_dev").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty())?;
    let pc = v.get("audio_pc").and_then(Value::as_bool) == Some(true);
    let how = match v.get("audio_route").and_then(Value::as_str) {
        Some("sink") if pc => Some("also on PC"),
        Some("sink") => Some("PC silent"),
        Some("loopback") => Some("PC speakers"),
        _ => None,
    };
    Some(match how {
        Some(h) => format!("Audio device: {dev} · {h}"),
        None => format!("Audio device: {dev}"),
    })
}

/// `3840x2160@60` -> `4K60`, `1920x1080@120` -> `1080p120`, `1920x1080` -> `1080p`. `None` when it is not a size.
pub(crate) fn picture_label(send_size: &str) -> Option<String> {
    let (wh, fps) = match send_size.trim().split_once('@') {
        Some((wh, fps)) => (wh, Some(fps.trim())),
        None => (send_size.trim(), None),
    };
    let (w, h) = wh.split_once('x')?;
    let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    let fps = match fps {
        Some(f) => f.parse::<u32>().ok()?.to_string(),
        None => String::new(),
    };
    Some(match (w, h) {
        (3840, 2160) => format!("4K{fps}"),
        (_, h) if w * 9 == h * 16 => format!("{h}p{fps}"),
        _ => format!("{w}×{h}{}", if fps.is_empty() { String::new() } else { format!(" @ {fps}") }),
    })
}

pub(crate) fn parse_stats(v: &Value) -> LiveStats {
    let f = |k: &str| v.get(k).and_then(Value::as_f64);
    let tv = v.get("fps_out").is_some() || v.get("fps_in").is_some();
    let size = v.get("send_size").and_then(Value::as_str).map(str::to_string);
    LiveStats {
        fps: f("fps_out").or_else(|| f("enc_fps")),
        latency_ms: f("e2e_ms"),
        latency_exact: v.get("e2e_src").and_then(Value::as_str) == Some("tv_clock"),
        decode_ms: f("decode_ms_p50"),
        mbps: f("recv_kbps").map(|k| k / 1000.0).or_else(|| f("send_mbps")),
        sent_mbps: f("send_mbps"),
        target_mbps: f("target_mbps"),
        rate: if v.get("fixed_rate").and_then(Value::as_bool) == Some(true) {
            RateState::Fixed
        } else {
            RateState::Ceiling
        },
        dropped: match (f("dropped"), f("late")) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        },
        rtt_ms: f("rtt_ms"),
        encoder: v.get("codec_out").and_then(Value::as_str).map(str::to_string),
        picture: size.as_deref().and_then(picture_label),
        size,
        tv,
        audio_line: audio_line(v),
        audio_device_line: audio_device_line(v),
        audio_route: v.get("audio_route").and_then(Value::as_str).map(str::to_string),
    }
}

/// A fixed rate counts as low under this share of its target.
pub(crate) const RATE_LOW_SHARE: f64 = 0.8;
/// ... for this many stats lines in a row (the sender writes one per second).
pub(crate) const RATE_LOW_SECS: u32 = 3;

/// Watches the sent rate against a fixed target, one stats line at a time.
#[derive(Debug, Default)]
struct RateWatch {
    low_lines: u32,
}

impl RateWatch {
    /// Count this line and mark `s.rate` [`RateState::Low`] when due. A ceiling (Auto) target never warns:
    /// sending less is by design.
    fn update(&mut self, s: &mut LiveStats) {
        let low = match (s.rate, s.sent_mbps, s.target_mbps) {
            (RateState::Fixed | RateState::Low, Some(sent), Some(target)) if target > 0.0 => {
                sent < target * RATE_LOW_SHARE
            }
            _ => false,
        };
        self.low_lines = if low { self.low_lines + 1 } else { 0 };
        if self.low_lines >= RATE_LOW_SECS {
            s.rate = RateState::Low;
        }
    }
}

// ------------------------------------------------------------------------------------------------ runner

/// What the runner reports, from its own thread.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StreamEvent {
    /// One stats line: the panel's numbers, and the whole line as the sender wrote it (for the logger).
    Stats(Box<LiveStats>, Value),
    /// TCP connected to the TV.
    Connected,
    /// Not connected; the sender keeps trying (`detail` is its reason).
    Waiting { detail: String },
    /// The connection dropped; the sender reconnects on its own.
    Disconnected,
    /// The sender changed or failed an encoder (`[encoder]` log lines).
    EncoderNotice(String),
    /// The sender says the monitor it captures renders on another GPU than NVIDIA.
    WrongGpu,
    /// The TV said which video decoders it has (its CAPS, once per connection).
    TvCaps(TvCaps),
    /// The sender exited unexpectedly and will be restarted after `delay`.
    Restarting { attempt: u32, delay: Duration, message: String },
    /// The sender failed too often, or cannot start at all. The runner has stopped.
    GaveUp { message: String },
}

/// What one stderr line means for the app.
fn classify(line: &str) -> Option<StreamEvent> {
    let l = line.trim();
    if l.starts_with("[warn]") && l.contains(WRONG_GPU) {
        return Some(StreamEvent::WrongGpu);
    }
    if l.starts_with("[net] connected") {
        Some(StreamEvent::Connected)
    } else if let Some(rest) = l.strip_prefix("[net] waiting for TV") {
        Some(StreamEvent::Waiting { detail: rest.trim().to_string() })
    } else if l.starts_with("[net] disconnected") || l.starts_with("[net] connection closed") {
        Some(StreamEvent::Disconnected)
    } else if l.starts_with(crate::tvcaps::LINE_PREFIX) {
        TvCaps::from_line(l).map(StreamEvent::TvCaps)
    } else if l.starts_with("[encoder]")
        && ["failed", "instead", "fall", "no working"].iter().any(|k| l.contains(k))
        && !l.contains("cached choice")
    {
        Some(StreamEvent::EncoderNotice(l.trim_start_matches("[encoder]").trim().to_string()))
    } else {
        None
    }
}

/// What the sender says (in its `Error:` line) when it cannot switch a reused virtual monitor to `--size`.
const MODE_SWITCH_FAILED: &str = "could not switch the TV monitor to ";

/// Errors that will not go away by retrying.
fn is_fatal(message: &str) -> bool {
    [
        "unknown argument",
        "needs a value",
        "is required",
        "is not supported",
        "not on PATH",
        "not found",
        MODE_SWITCH_FAILED,
    ]
    .iter()
    .any(|k| message.contains(k))
}

/// The size from a "could not switch the TV monitor to 3840x2160@60 (...)" error, or `None` for any other error.
pub(crate) fn mode_switch_failure(message: &str) -> Option<(u32, u32)> {
    let rest = &message[message.find(MODE_SWITCH_FAILED)? + MODE_SWITCH_FAILED.len()..];
    let size = rest.split(|c: char| c == '@' || c.is_whitespace()).next()?;
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs([1, 2, 4, 8, 10][usize::try_from(attempt.saturating_sub(1)).map_or(4, |i| i.min(4))])
}

/// Sender stderr goes to `%APPDATA%\TunnelRez\sender.log` (fresh every app start once it passes 2 MB).
#[derive(Debug)]
struct Log(Mutex<Option<File>>);

impl Log {
    fn open(path: &Path) -> Self {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 2 << 20) {
            let _ = std::fs::remove_file(path);
        }
        let file = path
            .parent()
            .and_then(|d| std::fs::create_dir_all(d).ok())
            .and_then(|()| OpenOptions::new().create(true).append(true).open(path).ok());
        Log(Mutex::new(file))
    }

    fn line(&self, line: &str) {
        if let Some(f) = self.0.lock().expect("log lock poisoned").as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// What to run.
#[derive(Clone, Debug)]
pub(crate) struct Launch {
    pub(crate) sender: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) log: PathBuf,
    /// The sender has `audio-restore`: run it when the sender had to be killed (it could not put back the default
    /// playback device or the mute itself).
    pub(crate) audio_restore: bool,
}

/// A running (and self-restarting) sender. Call [`Runner::stop`] for a clean stop; dropping it kills the sender.
#[derive(Debug)]
pub(crate) struct Runner {
    stop: Arc<AtomicBool>,
    /// Set by [`Drop`] before it kills the sender, so the supervisor knows the stop was not clean.
    killed: Arc<AtomicBool>,
    job: Arc<platform::Job>,
    thread: Option<JoinHandle<()>>,
}

enum Line {
    Out(String),
    Err(String),
}

impl Runner {
    /// Start the sender. `on_event` is called from the runner's threads.
    ///
    /// # Errors
    /// When the job object or the supervisor thread cannot be created.
    pub(crate) fn start(launch: Launch, on_event: impl Fn(StreamEvent) + Send + Sync + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let killed = Arc::new(AtomicBool::new(false));
        let job = Arc::new(platform::Job::new()?);
        let thread = {
            let (stop, killed, job) = (Arc::clone(&stop), Arc::clone(&killed), Arc::clone(&job));
            std::thread::Builder::new()
                .name("sender-supervisor".into())
                .spawn(move || supervise(&launch, &Flags { stop: &stop, killed: &killed }, &job, &on_event))
                .context("start sender supervisor")?
        };
        Ok(Runner { stop, killed, job, thread: Some(thread) })
    }

    /// Stop cleanly: Ctrl+Break (the sender sends BYE and exits), kill after [`STOP_GRACE`]. Blocks until done.
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        if self.thread.is_some() {
            // Not stopped cleanly: do not block in Drop, just kill. The supervisor, still running, sees `killed`
            // and runs `audio-restore` on its own thread.
            self.killed.store(true, Ordering::SeqCst);
            self.stop.store(true, Ordering::SeqCst);
            self.job.terminate();
        }
    }
}

fn spawn(launch: &Launch, job: &platform::Job) -> Result<(Child, mpsc::Receiver<Line>)> {
    let mut cmd = Command::new(&launch.sender);
    platform::hide_console_own_group(&mut cmd)
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = launch.sender.parent() {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("start {}", launch.sender.display()))?;
    if let Err(e) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(Line::Out(l)).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(Line::Err(l)).is_err() {
                    break;
                }
            }
        });
    }
    Ok((child, rx))
}

/// How the sender ended when it was asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// It exited on Ctrl+Break (it sent BYE and put the audio back itself).
    Clean,
    /// It had to be killed.
    Killed,
}

fn stop_child(child: &mut Child, job: &platform::Job) -> Ended {
    if platform::send_ctrl_break(child.id()).is_ok() {
        let until = Instant::now() + STOP_GRACE;
        while Instant::now() < until {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ended::Clean;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    job.terminate();
    let _ = child.wait();
    Ended::Killed
}

/// The runner's signals to its supervisor.
struct Flags<'a> {
    /// Stop the sender.
    stop: &'a AtomicBool,
    /// The sender was killed from outside (the [`Runner`] was dropped without a clean stop).
    killed: &'a AtomicBool,
}

/// Whether to run `audio-restore` after a stop that ended as `ended`.
fn restore_needed(launch: &Launch, ended: Ended, killed: &AtomicBool) -> bool {
    launch.audio_restore && (ended == Ended::Killed || killed.load(Ordering::SeqCst))
}

fn supervise(launch: &Launch, flags: &Flags<'_>, job: &platform::Job, on: &(impl Fn(StreamEvent) + Send + Sync)) {
    let stop = flags.stop;
    let log = Log::open(&launch.log);
    log.line(&format!("---- {} {}", launch.sender.display(), launch.args.join(" ")));
    let mut fails = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        let (mut child, lines) = match spawn(launch, job) {
            Ok(x) => x,
            Err(e) => {
                on(StreamEvent::GaveUp { message: format!("{e:#}") });
                return;
            }
        };
        let mut error = String::new();
        let mut hevc_missing: Option<String> = None;
        let mut watch = RateWatch::default();
        let mut handle = |line: Line, error: &mut String, hevc: &mut Option<String>| match line {
            Line::Out(l) => {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    let mut s = parse_stats(&v);
                    watch.update(&mut s);
                    on(StreamEvent::Stats(Box::new(s), v));
                }
            }
            Line::Err(l) => {
                log.line(&l);
                if l.starts_with("[encoder] probe failed") && crate::gate::needs_hevc_extension(&l) {
                    *hevc = Some(l.trim().to_string());
                }
                if let Some(msg) = l.strip_prefix("Error: ") {
                    *error = msg.trim().to_string();
                }
                if let Some(ev) = classify(&l) {
                    on(ev);
                }
            }
        };
        loop {
            match lines.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => handle(line, &mut error, &mut hevc_missing),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
            if stop.load(Ordering::SeqCst) {
                let ended = stop_child(&mut child, job);
                log.line("---- stopped");
                if restore_needed(launch, ended, flags.killed) {
                    match audio_restore(&launch.sender) {
                        Ok(out) => log.line(&format!("---- audio-restore: {out}")),
                        Err(e) => log.line(&format!("---- audio-restore: {e:#}")),
                    }
                }
                return;
            }
            if let Ok(Some(status)) = child.try_wait() {
                // Drain what is left so the last error line is not lost.
                while let Ok(line) = lines.recv_timeout(Duration::from_millis(200)) {
                    handle(line, &mut error, &mut hevc_missing);
                }
                if error.is_empty() {
                    error = format!("the sender exited ({status})");
                }
                if let Some(h) = hevc_missing.take() {
                    error = format!("{error} ({h})");
                }
                break;
            }
        }
        log.line(&format!("---- exited: {error}"));
        if started.elapsed() >= HEALTHY_RUN {
            fails = 0;
        }
        fails += 1;
        if fails >= MAX_FAILS || (is_fatal(&error) && started.elapsed() < HEALTHY_RUN) {
            on(StreamEvent::GaveUp { message: error });
            return;
        }
        let delay = backoff(fails);
        on(StreamEvent::Restarting { attempt: fails, delay, message: error });
        let until = Instant::now() + delay;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Bitrate, Picture};
    use serde_json::json;

    const HELP_MAIN: &str = "lrt-send --tv <ip> [--port 7878] [--monitor N | --extend] [--list-monitors]\n\
         [--codec h264|hevc|av1] [--encoder nvenc|qsv] [--bitrate 30M] [--fps 60] [--size 1920x1080]";
    /// The usage lines of the current sender (`tunnelrez-send --help`), verbatim.
    const HELP_ENCODERS: &str = "\
tunnelrez-send --tv <ip> [--port 7878] [--monitor N | --extend] [--codec h264|hevc|av1]
                [--backend ffmpeg|native] [--encoder auto|nvenc|qsv|amf|mf|x264|x265|svtav1]
                [--size WxH|native] [--fps 60] [--bitrate 30M] [--threads N] [--preset P]
                [--pacing dup|smooth|new] [--duration SECS] [--ffmpeg PATH] [--reprobe] [--verbose]
                [--capture auto|wgc|dda] [--intra-refresh N] [--sync-encode] [--vbv FRAMES]
                [--split auto|forced|two|off]
tunnelrez-send doctor [--codec C] [--monitor N | --extend] [--size WxH|native] [--fps 60]
tunnelrez-send --list-monitors

  --backend         ffmpeg or native (in-process capture -> NVENC SDK, H.264/HEVC, lowest latency).
                    Not given: decided by --encoder.
  --encoder         auto (default): the native NVENC backend when this build has it, the codec is
                    H.264/HEVC and an NVIDIA GPU renders the monitor; else ffmpeg: probe nvenc -> qsv
";

    #[test]
    fn help_of_main_sender() {
        let c = parse_help(HELP_MAIN);
        assert_eq!(c.encoders, ["nvenc", "qsv"]);
        assert!(c.backends.is_empty());
        assert_eq!(c.codecs, ["h264", "hevc", "av1"]);
        assert_eq!(c.encoder_choices(), [Encoder::Auto, Encoder::Nvenc, Encoder::Qsv]);
    }

    #[test]
    fn help_of_encoders_sender() {
        let c = parse_help(HELP_ENCODERS);
        assert_eq!(c.backends, ["ffmpeg", "native"]);
        assert_eq!(c.encoders, ["auto", "nvenc", "qsv", "amf", "mf", "x264", "x265", "svtav1"]);
        assert_eq!(c.codecs, ["h264", "hevc", "av1"], "`doctor [--codec C]` adds nothing");
        assert_eq!(
            c.encoder_choices(),
            [
                Encoder::Auto,
                Encoder::Nvenc,
                Encoder::NvencNative,
                Encoder::Qsv,
                Encoder::Amf,
                Encoder::Mf,
                Encoder::Cpu
            ]
        );
        let mf = Settings { tv_host: "tv".into(), encoder: Encoder::Mf, codec: Codec::Hevc, ..Default::default() };
        let a = sender_args(&mf, &c, &Source::Extend).join(" ");
        assert!(a.contains("--codec hevc --encoder mf"), "{a}");
    }

    #[test]
    fn args_default_extend() {
        let s = Settings { tv_host: "tv.lan".into(), ..Default::default() };
        let a = sender_args(&s, &parse_help(HELP_MAIN), &Source::Extend);
        assert_eq!(
            a,
            ["--tv", "tv.lan", "--extend", "--size", "1920x1080", "--fps", "60", "--codec", "hevc", "--bitrate", "40M"]
        );
    }

    #[test]
    fn args_mirror_4k_cpu_bitrate() {
        let s = Settings {
            tv_host: "tv".into(),
            picture: Picture::P2160x60,
            codec: Codec::H264,
            encoder: Encoder::Cpu,
            bitrate_mbps: Bitrate::Mbps(50),
            ..Default::default()
        };
        let a = sender_args(&s, &parse_help(HELP_ENCODERS), &Source::Monitor(2)).join(" ");
        assert_eq!(a, "--tv tv --monitor 2 --size 3840x2160 --fps 60 --codec h264 --encoder x264 --bitrate 50M");
    }

    #[test]
    fn args_bitrate_fixed_or_auto() {
        let fixed = Settings { tv_host: "tv".into(), bitrate_mbps: Bitrate::Mbps(150), ..Default::default() };
        let a = sender_args(&fixed, &parse_help(HELP_ENCODERS), &Source::Extend).join(" ");
        assert!(a.ends_with("--bitrate 150M"), "{a}");
        let auto = Settings { tv_host: "tv".into(), bitrate_mbps: Bitrate::Auto, ..Default::default() };
        let a = sender_args(&auto, &parse_help(HELP_ENCODERS), &Source::Extend);
        assert!(!a.iter().any(|x| x == "--bitrate"), "Auto leaves the rate to the sender: {a:?}");
    }

    #[test]
    fn stats_sent_and_target() {
        let s =
            parse_stats(&json!({"send_mbps": 148.9, "recv_kbps": 149_100, "target_mbps": 150.0, "fixed_rate": true}));
        assert_eq!((s.sent_mbps, s.target_mbps, s.rate), (Some(148.9), Some(150.0), RateState::Fixed));
        let old = parse_stats(&json!({"send_mbps": 30.0}));
        assert_eq!((old.target_mbps, old.rate), (None, RateState::Ceiling), "an older sender sends no target");
        assert_eq!(serde_json::to_value(RateState::Low).unwrap(), json!("low"));
    }

    #[test]
    fn rate_low_after_three_seconds_under_80_percent() {
        let line = |sent: f64, rate: RateState| LiveStats {
            sent_mbps: Some(sent),
            target_mbps: Some(100.0),
            rate,
            ..Default::default()
        };
        let mut w = RateWatch::default();
        let mut seen = Vec::new();
        for sent in [99.0, 79.0, 60.0, 31.0, 30.0, 85.0, 70.0] {
            let mut s = line(sent, RateState::Fixed);
            w.update(&mut s);
            seen.push(s.rate == RateState::Low);
        }
        assert_eq!(
            seen,
            [false, false, false, true, true, false, false],
            "3 low lines in a row warn; one good line clears"
        );
        let mut w = RateWatch::default();
        let mut s = line(80.0, RateState::Fixed);
        for _ in 0..5 {
            w.update(&mut s);
        }
        assert_eq!(s.rate, RateState::Fixed, "exactly 80% is not low");
        let mut w = RateWatch::default();
        let mut s = line(10.0, RateState::Ceiling);
        for _ in 0..5 {
            w.update(&mut s);
        }
        assert_eq!(s.rate, RateState::Ceiling, "a ceiling (Auto) never warns");
        let mut s = LiveStats { rate: RateState::Fixed, ..Default::default() };
        for _ in 0..5 {
            w.update(&mut s);
        }
        assert_eq!(s.rate, RateState::Fixed, "no numbers: no warning");
    }

    #[test]
    fn args_drop_unsupported_encoder() {
        let s = Settings { tv_host: "tv".into(), encoder: Encoder::Amf, ..Default::default() };
        let a = sender_args(&s, &parse_help(HELP_MAIN), &Source::Extend);
        assert!(!a.iter().any(|x| x == "--encoder"), "{a:?}");
    }

    /// Auto leaves the choice to the sender (native NVENC on an NVIDIA-rendered monitor, else ffmpeg NVENC, QSV,
    /// CPU), so neither `--encoder` nor `--backend` is passed, whatever the sender supports.
    #[test]
    fn args_auto_encoder_passes_no_encoder_or_backend() {
        for help in [HELP_MAIN, HELP_ENCODERS] {
            for picture in [Picture::P1080x60, Picture::P2160x60] {
                let s = Settings { tv_host: "tv".into(), encoder: Encoder::Auto, picture, ..Default::default() };
                let a = sender_args(&s, &parse_help(help), &Source::Extend);
                assert!(!a.iter().any(|x| x == "--encoder" || x == "--backend"), "{a:?}");
            }
        }
        let s = Settings { tv_host: "tv".into(), picture: Picture::P2160x60, ..Default::default() };
        let a = sender_args(&s, &parse_help(HELP_ENCODERS), &Source::Extend).join(" ");
        assert_eq!(a, "--tv tv --extend --size 3840x2160 --fps 60 --codec hevc --bitrate 100M");
    }

    #[test]
    fn sender_found_in_checkout() {
        let repo = std::env::temp_dir().join(format!("tunnelrez-find-{}", std::process::id()));
        let app_dir = repo.join("app").join("target").join("release");
        let sender_dir = repo.join("sender").join("target").join("release");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(&sender_dir).unwrap();
        let want = sender_dir.join("tunnelrez-send.exe");
        std::fs::write(&want, b"").unwrap();
        let found = sender_candidates(&app_dir).find(|p| p.is_file());
        // Next to the app wins over the checkout.
        let beside = app_dir.join("slipstream-send.exe");
        std::fs::write(&beside, b"").unwrap();
        let found_beside = sender_candidates(&app_dir).find(|p| p.is_file());
        let _ = std::fs::remove_dir_all(&repo);
        assert_eq!(found, Some(want));
        assert_eq!(found_beside, Some(beside));
    }

    #[test]
    fn args_native_backend() {
        let s = Settings { tv_host: "tv".into(), encoder: Encoder::NvencNative, ..Default::default() };
        let a = sender_args(&s, &parse_help(HELP_ENCODERS), &Source::Extend).join(" ");
        assert!(a.contains("--backend native"), "{a}");
        assert!(!a.contains("--encoder"), "{a}");
    }

    /// A sender with audio: the usage line lists the modes, and an option line starts with `  --audio`.
    const HELP_AUDIO: &str = "\
tunnelrez-send --tv <ip> [--port 7878] [--codec h264|hevc|av1] [--encoder auto|nvenc|qsv]
                [--size WxH|native] [--fps 60] [--bitrate 30M] [--audio on|off|tv-only]

  --audio           on (default): the PC's sound plays on the TV. off: no sound.
                    tv-only: also mutes this PC's speakers while streaming.
";

    #[test]
    fn help_with_and_without_audio() {
        assert_eq!(parse_help(HELP_AUDIO).audio, ["on", "off", "tv-only"]);
        assert!(parse_help(HELP_AUDIO).has_audio());
        assert!(!parse_help(HELP_ENCODERS).has_audio(), "an older sender");
        assert!(!parse_help(HELP_MAIN).has_audio());
        // The option line alone (no bracketed usage) is enough.
        let line_only = "tunnelrez-send --tv <ip>\n  --audio  on|off|tv-only  sound on the TV\n";
        assert_eq!(parse_help(line_only).audio, ["on", "off", "tv-only"]);
        // The codec/encoder lists are not disturbed by the `-` now allowed in a value.
        assert_eq!(parse_help(HELP_AUDIO).codecs, ["h264", "hevc", "av1"]);
        assert_eq!(parse_help(HELP_AUDIO).encoders, ["auto", "nvenc", "qsv"]);
    }

    #[test]
    fn args_audio_only_when_asked_and_supported() {
        use crate::settings::AudioMode;
        let caps = parse_help(HELP_AUDIO);
        let base = Settings { tv_host: "tv".into(), ..Default::default() };
        let args = |s: &Settings, c: &Caps| sender_args(s, c, &Source::Extend);
        // Default (on): the command line is what it was before audio existed, with or without audio support.
        assert_eq!(
            args(&base, &caps),
            ["--tv", "tv", "--extend", "--size", "1920x1080", "--fps", "60", "--codec", "hevc", "--bitrate", "40M"]
        );
        assert!(!args(&base, &caps).iter().any(|x| x == "--audio"), "on is the sender's default: no flag");
        let off = Settings { audio: AudioMode::Off, ..base.clone() };
        let tv_only = Settings { audio: AudioMode::TvOnly, ..base.clone() };
        let a = args(&off, &caps);
        assert_eq!(&a[a.len() - 2..], ["--audio", "off"]);
        let a = args(&tv_only, &caps);
        assert_eq!(&a[a.len() - 2..], ["--audio", "tv-only"]);
        // An older sender without --audio: never passed.
        let old = parse_help(HELP_ENCODERS);
        assert!(!args(&off, &old).iter().any(|x| x == "--audio"));
        assert!(!args(&tv_only, &old).iter().any(|x| x == "--audio"));
        assert_eq!(args(&off, &old), args(&base, &old), "the old sender gets the same line whatever the audio setting");
    }

    /// The new sender's usage line, `--audio-sink` option and subcommands (from `tunnelrez-send --help`).
    const HELP_SINK: &str = "\
tunnelrez-send --tv <ip> [--port 7878] [--codec h264|hevc|av1] [--encoder auto|nvenc|qsv]
                [--split auto|forced|two|off] [--fixed-rate on|off] [--audio on|off|tv-only]
                [--audio-sink auto|off|NAME]
tunnelrez-send --list-monitors

  --audio           on (default): the PC's sound plays on the TV too. off: video only.
  --audio-sink      auto (default): if a virtual audio device is present, make it the Windows default
  audio-probe       check that loopback capture survives a muted PC (silent; mute/volume restored)
  audio-restore     put back a default playback device or mute a killed sender left behind
  audio-devices     one JSON line: the playback devices, and the virtual sink auto would use
  audio-sink        status (one JSON line) | install [--dry-run] | uninstall [--force]: the TV speaker
";

    #[test]
    fn vdd_status_json() {
        let line = r#"{"adapter":{"problem":0,"state":"ok"},"cause":"none","driver_installed":true,
            "driver_version":"0.45.0.0","elevated":false,"installer":"C:\\k\\p.exe","next":"none","ready":true,
            "render_on_nvidia":{"applies":true,"ok":true}}"#
            .replace('\n', " ");
        let s = parse_vdd_status(&format!("noise\n{line}\n")).unwrap();
        assert!(s.ready && s.render_on_nvidia.applies, "{s:?}");
        assert_eq!((s.cause.as_str(), s.driver_version.as_deref()), ("none", Some("0.45.0.0")));
        assert_eq!(s.installer.as_deref(), Some(r"C:\k\p.exe"));
        let s = parse_vdd_status(r#"{"cause":"missing","driver_version":null,"installer":null}"#).unwrap();
        assert!(!s.ready && s.installer.is_none());
        assert_eq!(parse_vdd_status(r#"{"device":true}"#), None, "the speaker's status is not this");
    }

    #[test]
    fn vdd_commands() {
        assert_eq!(VddFix::Repair.args(), ["vdd", "repair"]);
        assert_eq!(VddFix::RepairRestart.args(), ["vdd", "repair", "--restart-adapter"]);
        assert_eq!(VddFix::Install.args(), ["vdd", "install"]);
        assert_eq!(VddFix::Reinstall.args(), ["vdd", "install", "--force"]);
        assert!(parse_help("  vdd               status (one JSON line) | repair\n").tv_monitor);
        assert!(!parse_help(HELP_SINK).tv_monitor, "an older sender");
    }

    #[test]
    fn the_wrong_gpu_warning_is_seen() {
        let line = r"[warn] \\.\DISPLAY10 is not on the NVIDIA GPU: no native NVENC path. For the Parsec ...";
        assert_eq!(classify(line), Some(StreamEvent::WrongGpu));
        assert_eq!(classify("[warn] something else"), None);
    }

    #[test]
    fn help_with_and_without_audio_sink() {
        let c = parse_help(HELP_SINK);
        assert!(c.audio_sink && c.audio_devices && c.audio_restore && c.tv_speaker, "{c:?}");
        assert_eq!(c.audio, ["on", "off", "tv-only"], "--audio-sink does not disturb the --audio modes");
        assert_eq!(c.encoders, ["auto", "nvenc", "qsv"]);
        for old in [HELP_AUDIO, HELP_ENCODERS, HELP_MAIN] {
            let c = parse_help(old);
            assert!(!c.audio_sink && !c.audio_devices && !c.audio_restore && !c.tv_speaker, "an older sender: {c:?}");
        }
        // A word inside a sentence is not a command.
        assert!(!has_command("  --x   see audio-restore for more", "audio-restore"));
        assert!(!has_command("  audio-restored  no", "audio-restore"));
        // The `--audio-sink` option and its usage are not the `audio-sink` command.
        let flag_only: Vec<&str> = HELP_SINK.lines().filter(|l| !l.starts_with("  audio-sink ")).collect();
        let c = parse_help(&flag_only.join("\n"));
        assert!(c.audio_sink && !c.tv_speaker, "{c:?}");
        assert!(parse_help("tunnelrez-send audio-sink status|install|uninstall\n").tv_speaker, "the usage line form");
        assert!(!parse_help("tunnelrez-send audio-sink status|install|uninstall\n").tv_speaker_restore, "older");
        assert!(parse_help("  audio-sink  ... restore-defaults: put back\n").tv_speaker_restore);
    }

    #[test]
    fn sink_status_json() {
        let all = r#"{"device":true,"elevated":false,"endpoint":"Speakers (Steam Streaming Speakers)","inf":"C:\\x\\SteamStreamingSpeakers.inf","installed_by_tunnelrez":true,"source":"bundled"}"#;
        let st = parse_sink_status(&format!("[audio] something\n{all}\n")).expect("parses");
        let want = SinkStatus {
            device: true,
            endpoint: Some("Speakers (Steam Streaming Speakers)".into()),
            inf: Some(r"C:\x\SteamStreamingSpeakers.inf".into()),
            source: Some("bundled".into()),
            installed_by_tunnelrez: true,
            elevated: false,
            ready: None,
            reboot_required: None,
        };
        assert_eq!(st, want);
        // An older sender (no `ready`, no `reboot_required`): ready = a device with an endpoint, no restart.
        assert!(st.is_ready() && !st.needs_reboot());
        let old = parse_sink_status(r#"{"device":true,"endpoint":null,"inf":"a.inf"}"#).expect("parses");
        assert!(!old.is_ready() && !old.needs_reboot(), "a device without its endpoint yet");
        // A device without its endpoint, waiting for a restart.
        let reboot = r#"{"device":true,"endpoint":null,"inf":"a.inf","ready":false,"reboot_required":true}"#;
        let st = parse_sink_status(reboot).expect("parses");
        assert!(st.device && !st.is_ready() && st.needs_reboot(), "{st:?}");
        // ... and one that only waits for Windows (no restart).
        let waiting = r#"{"device":true,"endpoint":null,"inf":"a.inf","ready":false,"reboot_required":false}"#;
        let st = parse_sink_status(waiting).expect("parses");
        assert!(st.device && !st.is_ready() && !st.needs_reboot(), "{st:?}");
        // The sender's `ready` wins over the guess.
        let st = parse_sink_status(r#"{"device":true,"endpoint":"X","ready":false}"#).expect("parses");
        assert!(!st.is_ready());
        // Null and missing fields: no device, no driver files.
        let nulls = r#"{"device":false,"elevated":null,"endpoint":null,"inf":null,"installed_by_tunnelrez":null,"source":null}"#;
        assert_eq!(parse_sink_status(nulls), Some(SinkStatus::default()));
        assert_eq!(parse_sink_status(r#"{"device":null}"#), Some(SinkStatus::default()));
        let some = parse_sink_status(r#"{"device":false,"inf":"a.inf","future":1}"#).expect("unknown keys are fine");
        assert_eq!((some.device, some.inf.as_deref(), some.source), (false, Some("a.inf"), None));
        // Garbage: nothing.
        let garbage = ["", "Error: no sender\n", "{", "[1,2]", "{}", r#"{"auto":null,"devices":[]}"#];
        for text in garbage.into_iter().chain([r#"{"device":"yes"}"#]) {
            assert_eq!(parse_sink_status(text), None, "{text:?}");
        }
    }

    #[test]
    #[ignore = "runs the sender built in this checkout (status only reads)"]
    fn real_sink_status() {
        let sender = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\sender\target\release\tunnelrez-send.exe");
        let caps = probe(&sender).expect("--help");
        assert!(caps.tv_speaker, "{caps:?}");
        let st = sink_status(&sender).expect("audio-sink status");
        assert!(st.source.is_some() == st.inf.is_some(), "{st:?}");
    }

    #[test]
    fn sink_install_command_is_the_install_subcommand() {
        let cmd = sink_install_command(Path::new(r"C:\TunnelRez\tunnelrez-send.exe"));
        assert_eq!(cmd.get_program(), r"C:\TunnelRez\tunnelrez-send.exe");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["audio-sink", "install"]);
    }

    #[test]
    fn install_output_restart() {
        assert!(install_needs_reboot("installed the TV speaker (REBOOT_REQUIRED: restart Windows to finish)"));
        assert!(!install_needs_reboot("installed the TV speaker"));
        assert!(!install_needs_reboot(""));
    }

    #[test]
    fn sink_install_error_words() {
        let stderr = "[audio-sink] relaunching elevated\nError: the Windows prompt was declined\n";
        assert_eq!(sink_install_error(stderr, "", Some(1)), "the Windows prompt was declined");
        assert_eq!(sink_install_error("pnputil: 0x5\n\n", "", Some(2)), "pnputil: 0x5", "the last line");
        assert_eq!(sink_install_error("", "half done\n", Some(3)), "half done");
        assert_eq!(sink_install_error("Error:   \n", "", Some(4)), "Error:", "an empty error: the line itself");
        assert_eq!(sink_install_error("", "", Some(5)), "the install failed (exit code 5)");
        assert_eq!(sink_install_error("", "", None), "the install was stopped");
    }

    #[test]
    fn args_audio_sink_only_when_set_and_supported() {
        let caps = parse_help(HELP_SINK);
        let base = Settings { tv_host: "tv".into(), ..Default::default() };
        let args = |s: &Settings, c: &Caps| sender_args(s, c, &Source::Extend);
        assert_eq!(
            args(&base, &caps),
            ["--tv", "tv", "--extend", "--size", "1920x1080", "--fps", "60", "--codec", "hevc", "--bitrate", "40M"],
            "auto (empty): the command line is unchanged"
        );
        let off = Settings { audio_sink: "off".into(), ..base.clone() };
        let a = args(&off, &caps);
        assert_eq!(&a[a.len() - 2..], ["--audio-sink", "off"]);
        let named = Settings { audio_sink: "Speakers (Steam Streaming Speakers)".into(), ..base.clone() };
        let a = args(&named, &caps);
        assert_eq!(&a[a.len() - 2..], ["--audio-sink", "Speakers (Steam Streaming Speakers)"], "one argument");
        let tv_only = Settings { audio: crate::settings::AudioMode::TvOnly, ..named.clone() };
        assert!(args(&tv_only, &caps)
            .join(" ")
            .ends_with("--audio tv-only --audio-sink Speakers (Steam Streaming Speakers)"));
        // A sender without the flag: never passed.
        let old = parse_help(HELP_AUDIO);
        assert_eq!(args(&named, &old), args(&base, &old));
    }

    #[test]
    fn audio_devices_json() {
        let text = r#"{"auto":"{0.0.0.00000000}.{b}","devices":[{"adapter":"Realtek(R) Audio","id":"{0.0.0.00000000}.{a}","name":"Speakers (Realtek(R) Audio)"},{"adapter":"Steam Streaming Speakers","id":"{0.0.0.00000000}.{b}","name":"Speakers (Steam Streaming Speakers)"}]}"#;
        let d = parse_audio_devices(&format!("[audio] something\n{text}\n")).expect("parses");
        assert_eq!(d.devices.len(), 2);
        assert_eq!(d.devices[1].adapter, "Steam Streaming Speakers");
        assert_eq!(d.auto_name(), Some("Speakers (Steam Streaming Speakers)"));
        let none = parse_audio_devices(r#"{"auto":null,"devices":[{"id":"x","name":"Speakers"}]}"#).expect("parses");
        assert_eq!((none.auto.as_deref(), none.auto_name()), (None, None), "no virtual device");
        assert_eq!(none.devices[0].adapter, "", "adapter is optional");
        assert_eq!(parse_audio_devices("Error: no audio\n"), None);
        assert_eq!(parse_audio_devices(""), None);
        // An auto id that is not in the list names nothing.
        let odd = AudioDevices { devices: vec![], auto: Some("x".into()) };
        assert_eq!(odd.auto_name(), None);
    }

    #[test]
    fn audio_device_line_words() {
        let line = |v: Value| parse_stats(&v).audio_device_line;
        assert_eq!(
            line(json!({"audio_dev": "Speakers (Steam Streaming Speakers)", "audio_route": "sink", "audio_pc": true}))
                .as_deref(),
            Some("Audio device: Speakers (Steam Streaming Speakers) · also on PC")
        );
        assert_eq!(
            line(json!({"audio_dev": "Steam Streaming Speakers", "audio_route": "sink", "audio_pc": false})).as_deref(),
            Some("Audio device: Steam Streaming Speakers · PC silent")
        );
        assert_eq!(
            line(json!({"audio_dev": "Speakers (Realtek(R) Audio)", "audio_route": "loopback", "audio_pc": false}))
                .as_deref(),
            Some("Audio device: Speakers (Realtek(R) Audio) · PC speakers")
        );
        assert_eq!(line(json!({"audio_dev": "X"})).as_deref(), Some("Audio device: X"), "no route said");
        assert_eq!(line(json!({"audio_dev": "  "})), None);
        assert_eq!(line(json!({"audio": "on"})), None, "an older sender");
        let s = parse_stats(&json!({"audio_dev": "X", "audio_route": "loopback"}));
        assert_eq!(s.audio_route.as_deref(), Some("loopback"));
        assert_eq!(parse_stats(&json!({})).audio_route, None);
    }

    #[test]
    fn restore_only_after_a_kill_and_with_a_sender_that_can() {
        let launch =
            |audio_restore| Launch { sender: PathBuf::new(), args: vec![], log: PathBuf::new(), audio_restore };
        let (yes, no) = (AtomicBool::new(true), AtomicBool::new(false));
        assert!(restore_needed(&launch(true), Ended::Killed, &no));
        assert!(restore_needed(&launch(true), Ended::Clean, &yes), "dropped: the Runner killed it");
        assert!(!restore_needed(&launch(true), Ended::Clean, &no), "a clean stop restores itself");
        assert!(!restore_needed(&launch(false), Ended::Killed, &yes), "an older sender has no audio-restore");
    }

    #[test]
    fn audio_line_words() {
        let line = |v: Value| parse_stats(&v).audio_line;
        assert_eq!(line(json!({"fps_out": 60.0})), None, "no audio key: nothing extra");
        assert_eq!(line(json!({"audio": "off"})), None);
        assert_eq!(
            line(json!({"audio": "on", "audio_e2e_ms": 45.2, "av_ms": 12.4, "a_buf_ms": 28.0})).as_deref(),
            Some("Audio: on · 45 ms · A/V +12 ms · buf 28 ms")
        );
        assert_eq!(
            line(json!({"audio": "on", "audio_tv_only": true, "audio_e2e_ms": 45.0, "av_ms": -3.4, "a_buf_ms": 28.0}))
                .as_deref(),
            Some("Audio: on · TV only · 45 ms · A/V -3 ms · buf 28 ms")
        );
        assert_eq!(line(json!({"audio": "on", "av_ms": 0.2})).as_deref(), Some("Audio: on · A/V 0 ms"));
        assert_eq!(line(json!({"audio": "on"})).as_deref(), Some("Audio: on"), "no numbers yet");
        assert_eq!(line(json!({"audio": "tv_no_audio"})).as_deref(), Some("Audio: TV has no audio support"));
        assert_eq!(line(json!({"audio": "waiting"})).as_deref(), Some("Audio: waiting for the TV"));
        assert_eq!(line(json!({"audio": "error"})).as_deref(), Some("Audio: error"));
        assert_eq!(line(json!({"audio": "something_new"})), None, "an unknown state: nothing");
    }

    #[test]
    fn monitors_both_formats() {
        let text = "Monitors (index for --monitor; ...):\n\
            \x20 [0] \\\\.\\DISPLAY1     2560x1600 at (0,0)  primary\n\
            \x20       igpu: adapter 0 Intel output 0 dup=ok\n\
            \x20 [1] \\\\.\\DISPLAY10    1920x1080 at (2560,0)  virtual(Parsec VDD)\n\
            Capture view (power-saving process = ffmpeg-igpu.exe ddagrab):\n\
            \x20 [2] \\\\.\\DISPLAY3     3840x2160 at (-3840,0)  adapter 1 NVIDIA output 0  dup=ok\n";
        let m = parse_monitors(text);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].device, r"\\.\DISPLAY1");
        assert!(m[0].primary && !m[0].is_virtual);
        assert!(m[1].is_virtual && !m[1].primary);
        assert_eq!((m[2].index, m[2].width, m[2].height), (2, 3840, 2160));
    }

    #[test]
    fn stats_with_and_without_tv() {
        let tv = parse_stats(&json!({"fps_out": 59.9, "e2e_ms": 18.4, "e2e_src": "tv_clock", "decode_ms_p50": 4.1,
            "recv_kbps": 29800, "dropped": 1, "late": 2, "enc_fps": 60.0, "codec_out": "hevc/nvenc"}));
        assert_eq!(tv.fps, Some(59.9));
        assert!(tv.latency_exact && tv.tv);
        assert_eq!(tv.mbps, Some(29.8));
        assert_eq!(tv.dropped, Some(3.0));
        assert_eq!(tv.encoder.as_deref(), Some("hevc/nvenc"));
        let pc =
            parse_stats(&json!({"enc_fps": 60.0, "send_mbps": 21.0, "e2e_ms": 9.0, "e2e_src": "est:cap2tx+rtt/2"}));
        assert_eq!(pc.fps, Some(60.0));
        assert!(!pc.latency_exact && !pc.tv);
        assert_eq!(pc.mbps, Some(21.0));
        assert_eq!(pc.dropped, None);
        assert_eq!(pc.picture, None, "an older sender sends no size");
    }

    #[test]
    fn stats_real_size() {
        let s = parse_stats(&json!({"enc_fps": 60.0, "codec_out": "hevc/native nvenc", "send_size": "3840x2160@60"}));
        assert_eq!(s.size.as_deref(), Some("3840x2160@60"));
        assert_eq!(s.picture.as_deref(), Some("4K60"));
        assert_eq!(picture_label("1920x1080@60").as_deref(), Some("1080p60"));
        assert_eq!(picture_label("1920x1080@120").as_deref(), Some("1080p120"));
        assert_eq!(picture_label("2560x1440@60").as_deref(), Some("1440p60"));
        assert_eq!(picture_label("1920x1080").as_deref(), Some("1080p"));
        assert_eq!(picture_label("2560x1600@60").as_deref(), Some("2560×1600 @ 60"));
        assert_eq!(picture_label("native"), None);
    }

    #[test]
    fn mode_switch_error() {
        let msg = r"could not switch the TV monitor to 3840x2160@60 (\\.\DISPLAY10): DXGI still shows 1920x1080";
        assert_eq!(mode_switch_failure(msg), Some((3840, 2160)));
        assert!(is_fatal(msg), "not transient: no restart loop");
        assert_eq!(mode_switch_failure("could not switch the TV monitor to 2560x1440 (x)"), Some((2560, 1440)));
        assert_eq!(mode_switch_failure("plug virtual display: no driver"), None);
    }

    #[test]
    fn log_lines() {
        assert_eq!(classify("[net] connected to 192.0.2.2:7878"), Some(StreamEvent::Connected));
        assert!(matches!(classify("[net] waiting for TV at x (refused)"), Some(StreamEvent::Waiting { .. })));
        assert_eq!(classify("[net] disconnected; reconnecting"), Some(StreamEvent::Disconnected));
        assert_eq!(
            classify("[encoder] no working hevc encoder; using h264 instead"),
            Some(StreamEvent::EncoderNotice("no working hevc encoder; using h264 instead".into()))
        );
        assert_eq!(classify("[encoder] cached choice: hevc nvenc (--reprobe to probe again)"), None);
        let caps = r#"[tv] caps {"v":1,"caps":[],"decoders":[{"name":"OMX.MTK.VIDEO.DECODER.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":60,"hw":true,"perf":["3840x2160@60"]}]}"#;
        match classify(caps) {
            Some(StreamEvent::TvCaps(c)) => assert!(c.decodes(Codec::Av1, Picture::P2160x60)),
            other => panic!("{other:?}"),
        }
        assert_eq!(classify("[tv] caps unreadable: EOF"), None);
        assert_eq!(classify("[capture] \\\\.\\DISPLAY10 1920x1080"), None);
    }

    #[test]
    fn retry_policy() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(9), Duration::from_secs(10));
        assert!(is_fatal("unknown argument --backend"));
        assert!(!is_fatal("connection reset"));
    }
}
