//! The virtual audio sink, as Sunshine and Apollo do it: while audio streams, a virtual playback device (the
//! "TV speaker", Steam Streaming Speakers, see [`super::driver`]; or another known one) becomes the Windows
//! default for all three roles, so everything the PC plays goes there, and the sender captures it (WASAPI loopback). The PC speakers are simply not the default, so "Play on
//! TV only" needs no mute. When audio stops, the old defaults come back.
//!
//! A restore record (`%LOCALAPPDATA%\TunnelRez\audio-route.json`) is written before any default changes, so a
//! killed sender is undone at the next start (or by `tunnelrez-send audio-restore`, which the app runs on quit).
//!
//! The switch is held for the whole sender process, not per TCP session: an encoder fallback or a reconnect
//! starts a new session within a second, and flipping the PC's default device each time would interrupt every
//! app's sound. The last session to end starts a short grace period; the defaults come back after it, or at once
//! when the sender exits.

use super::policy::{self, Policy};
use super::tv_only::{state_path, write_atomic};
use super::wasapi::{self, Com};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia, ERole};

/// Known virtual sinks, in order of preference. Matched (case-insensitive) as part of the device's friendly
/// name or its adapter name.
pub const KNOWN_SINKS: [&str; 4] =
    ["Steam Streaming Speakers", "CABLE Input (VB-Audio Virtual Cable)", "Virtual Audio Driver", "TunnelRez TV"];

/// After the last audio session ends, the defaults (and a "Play on TV only" mute) come back after this long
/// (a reconnect is faster).
pub(super) const GRACE: Duration = Duration::from_secs(3);

/// One playback device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Endpoint ID (`{0.0.0.00000000}.{guid}`).
    pub id: String,
    /// Friendly name, e.g. `CABLE Input (VB-Audio Virtual Cable)`.
    pub name: String,
    /// The adapter's name, e.g. `Steam Streaming Speakers`.
    pub adapter: String,
}

/// `--audio-sink`: which virtual device to stream through.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SinkChoice {
    /// The first [`KNOWN_SINKS`] device present.
    #[default]
    Auto,
    /// Never: loopback of the current default (and the mute for "Play on TV only").
    Off,
    /// A device by endpoint ID or name.
    Named(String),
}

impl SinkChoice {
    /// `auto`, `off`, or a device name or ID.
    pub fn parse(s: &str) -> SinkChoice {
        match s.trim() {
            "" | "auto" => SinkChoice::Auto,
            "off" => SinkChoice::Off,
            x => SinkChoice::Named(x.to_string()),
        }
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && hay.to_lowercase().contains(&needle.to_lowercase())
}

/// The sink to use among `devices`. A named choice matches the endpoint ID, then the exact friendly name, then
/// part of a name; `auto` takes the first [`KNOWN_SINKS`] entry that is present.
pub fn pick<'a>(devices: &'a [DeviceInfo], choice: &SinkChoice) -> Option<&'a DeviceInfo> {
    match choice {
        SinkChoice::Off => None,
        SinkChoice::Named(n) => devices
            .iter()
            .find(|d| d.id.eq_ignore_ascii_case(n))
            .or_else(|| devices.iter().find(|d| d.name.eq_ignore_ascii_case(n)))
            .or_else(|| devices.iter().find(|d| contains_ci(&d.name, n) || contains_ci(&d.adapter, n))),
        SinkChoice::Auto => KNOWN_SINKS
            .iter()
            .find_map(|k| devices.iter().find(|d| contains_ci(&d.name, k) || contains_ci(&d.adapter, k))),
    }
}

/// The three default-device roles Windows keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Console, Role::Multimedia, Role::Communications];

    fn key(self) -> &'static str {
        match self {
            Role::Console => "console",
            Role::Multimedia => "multimedia",
            Role::Communications => "communications",
        }
    }

    pub(super) fn erole(self) -> ERole {
        match self {
            Role::Console => eConsole,
            Role::Multimedia => eMultimedia,
            Role::Communications => eCommunications,
        }
    }
}

/// What the switch needs from Windows (mocked in tests).
pub trait System {
    /// Active playback devices.
    ///
    /// # Errors
    ///
    /// The list cannot be read.
    fn devices(&mut self) -> Result<Vec<DeviceInfo>>;
    fn default_id(&mut self, role: Role) -> Option<String>;
    /// # Errors
    ///
    /// Windows refused, or the device is gone.
    fn set_default(&mut self, id: &str, role: Role) -> Result<()>;
    /// # Errors
    ///
    /// No stored format.
    fn device_format(&mut self, id: &str) -> Result<Vec<u8>>;
    /// # Errors
    ///
    /// The device does not take the format.
    fn set_device_format(&mut self, id: &str, format: &[u8]) -> Result<()>;
    /// Another process `pid` still runs (a sender whose record must not be touched).
    fn process_alive(&mut self, pid: u32) -> bool;
}

/// The real Windows side. COM stays initialized on the creating thread while it lives.
#[derive(Debug)]
pub struct Windows {
    policy: Policy,
    _com: Com,
}

impl Windows {
    /// # Errors
    ///
    /// COM or the policy-config class is unavailable.
    pub fn new() -> Result<Windows> {
        let com = Com::init()?;
        Ok(Windows { policy: Policy::new()?, _com: com })
    }
}

impl System for Windows {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>> {
        wasapi::render_devices()
    }
    fn default_id(&mut self, role: Role) -> Option<String> {
        wasapi::default_id(role.erole())
    }
    fn set_default(&mut self, id: &str, role: Role) -> Result<()> {
        self.policy.set_default(id, role.erole())
    }
    fn device_format(&mut self, id: &str) -> Result<Vec<u8>> {
        self.policy.device_format(id)
    }
    fn set_device_format(&mut self, id: &str, format: &[u8]) -> Result<()> {
        self.policy.set_device_format(id, format)
    }
    fn process_alive(&mut self, pid: u32) -> bool {
        crate::winutil::process_alive(pid)
    }
}

/// What to undo: the defaults before the switch and the sink's format before it was set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub sink: String,
    pub sink_name: String,
    /// Each role's default before the switch (roles without a default are left out).
    pub before: Vec<(Role, String)>,
    /// The sink's device format before it was changed (raw `WAVEFORMATEX` bytes).
    pub format: Option<Vec<u8>>,
    /// The sender process that made the switch (a live one is not undone by another process's recovery).
    pub pid: u32,
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Record {
    fn to_json(&self) -> String {
        let before: Map<String, Value> = self.before.iter().map(|(r, id)| (r.key().to_string(), json!(id))).collect();
        json!({
            "sink": self.sink, "sink_name": self.sink_name, "before": before,
            "format": self.format.as_deref().map(hex), "pid": self.pid,
        })
        .to_string()
    }

    fn parse(s: &str) -> Option<Record> {
        let v: Value = serde_json::from_str(s).ok()?;
        let b = v.get("before")?.as_object()?;
        Some(Record {
            sink: v.get("sink")?.as_str()?.to_string(),
            sink_name: v.get("sink_name").and_then(Value::as_str).unwrap_or_default().to_string(),
            before: Role::ALL.iter().filter_map(|r| Some((*r, b.get(r.key())?.as_str()?.to_string()))).collect(),
            format: v.get("format").and_then(Value::as_str).and_then(unhex),
            pid: v.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()).unwrap_or(0),
        })
    }

    /// The PC device the old console default was (where "Also play on PC" plays); None when the sink was
    /// already the default.
    pub fn pc_device(&self) -> Option<&str> {
        self.before.iter().find(|(r, id)| *r == Role::Console && *id != self.sink).map(|(_, id)| id.as_str())
    }
}

/// Refuse when the route record at `path` belongs to another sender that still runs: its route is not ours.
///
/// # Errors
///
/// Another live sender owns that record.
pub fn refuse_live_route(sys: &mut dyn System, path: &Path, pid: u32) -> Result<()> {
    let rec = std::fs::read_to_string(path).ok().and_then(|t| Record::parse(&t));
    if let Some(p) = rec.filter(|p| p.pid != pid && sys.process_alive(p.pid)) {
        anyhow::bail!("another sender (pid {}) is streaming through {}", p.pid, p.sink_name);
    }
    Ok(())
}

/// Make `sink` the default for every role. The record is on disk at `path` before anything changes; on failure
/// whatever was changed is put back.
///
/// # Errors
///
/// The record cannot be written, or Windows refused a default change.
pub fn engage(sys: &mut dyn System, sink: &DeviceInfo, path: &Path, pid: u32) -> Result<Record> {
    // A record still on disk is a restore that has not finished yet: a role still on its sink keeps that
    // record's original device, so the new record does not lose it.
    let mut pending = std::fs::read_to_string(path).ok().and_then(|t| Record::parse(&t));
    if let Some(p) = pending.as_ref().filter(|p| p.pid != pid && sys.process_alive(p.pid)) {
        // Another sender is streaming through it right now: its route and record are not ours to touch.
        anyhow::bail!("another sender (pid {}) is streaming through {}", p.pid, p.sink_name);
    }
    // Finish it first if Windows lets us now.
    if let Some(p) = &pending {
        if restore(sys, p).1 {
            let _ = std::fs::remove_file(path);
            pending = None;
        }
    }
    if let Some(p) = pending.as_ref().filter(|p| p.sink != sink.id) {
        // A different sink still has an unfinished restore: switching to another one would leave two to undo.
        anyhow::bail!("the restore of {} from an earlier run is not finished yet", p.sink_name);
    }
    let before: Vec<(Role, String)> = Role::ALL
        .iter()
        .filter_map(|r| {
            let now = sys.default_id(*r)?;
            let original = pending
                .as_ref()
                .filter(|p| p.sink == now)
                .and_then(|p| p.before.iter().find(|(pr, _)| pr == r).map(|(_, id)| id.clone()));
            Some((*r, original.unwrap_or(now)))
        })
        .collect();
    let target = policy::stereo_48k_s16();
    let pending_format = pending.as_ref().and_then(|p| p.format.clone());
    let from_pending = pending_format.is_some();
    let old_format = pending_format.or_else(|| sys.device_format(&sink.id).ok()).filter(|f| *f != target);
    let mut rec =
        Record { sink: sink.id.clone(), sink_name: sink.name.clone(), before, format: old_format.clone(), pid };
    write_atomic(path, &rec.to_json()).with_context(|| format!("write {}", path.display()))?;
    if old_format.is_some() {
        if let Err(e) = sys.set_device_format(&sink.id, &target) {
            eprintln!("[audio] {} keeps its own format ({e:#}); the sender converts to 48 kHz s16 stereo", sink.name);
            if !from_pending {
                // Nothing was changed, so nothing to put back. (An earlier run's format stays pending.)
                rec.format = None;
                let _ = write_atomic(path, &rec.to_json());
            }
        }
    }
    for r in Role::ALL {
        if rec.before.iter().any(|(br, id)| *br == r && *id == sink.id) {
            continue;
        }
        if let Err(e) = sys.set_default(&sink.id, r) {
            let (msg, all) = restore(sys, &rec);
            if all {
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
            // Something still points at the sink: the record stays for `audio-restore` or the next start.
            return Err(e.context(format!("and the rollback was incomplete: {msg}")));
        }
    }
    Ok(rec)
}

/// Put the defaults back: only roles that still point at the sink (a device the user chose since then stays).
/// Then the sink's old format. Returns a log line, and whether everything that should be undone was.
pub fn restore(sys: &mut dyn System, rec: &Record) -> (String, bool) {
    use std::fmt::Write;
    let (mut back, mut kept, mut failed) = (0, 0, Vec::new());
    for (r, old) in &rec.before {
        if *old == rec.sink {
            continue;
        }
        let now = sys.default_id(*r);
        if now.as_deref() == Some(old.as_str()) {
            back += 1; // already back (Windows moves console and multimedia together)
            continue;
        }
        if now.as_deref() != Some(rec.sink.as_str()) {
            kept += 1;
            continue;
        }
        match sys.set_default(old, *r) {
            Ok(()) => back += 1,
            Err(e) => failed.push(format!("{}: {e:#}", r.key())),
        }
    }
    if let Some(f) = &rec.format {
        if let Err(e) = sys.set_device_format(&rec.sink, f) {
            failed.push(format!("the format of {}: {e:#}", rec.sink_name));
        }
    }
    let mut msg = format!("default playback device restored from {} ({back} role(s)", rec.sink_name);
    if kept > 0 {
        let _ = write!(msg, ", {kept} left as the user changed them");
    }
    msg.push(')');
    if !failed.is_empty() {
        let _ = write!(msg, "; could not restore {} (trying again next start)", failed.join("; "));
    }
    (msg, failed.is_empty())
}

/// `%LOCALAPPDATA%\TunnelRez\audio-route.json`.
fn route_path() -> Option<std::path::PathBuf> {
    state_path("audio-route.json")
}

/// Undo a switch an earlier sender left (its record file is there), unless that sender still runs. Returns a
/// log line when there was a record.
pub fn recover_stale() -> Option<String> {
    let path = route_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let Some(rec) = Record::parse(&text) else {
        let _ = std::fs::remove_file(&path);
        return Some("ignored an unreadable audio route file".into());
    };
    if rec.pid != std::process::id() && crate::winutil::process_alive(rec.pid) {
        return Some(format!("{} is the default for a sender that still runs (pid {})", rec.sink_name, rec.pid));
    }
    let r = std::thread::spawn(move || -> Result<(String, bool)> {
        let mut sys = Windows::new()?;
        Ok(restore(&mut sys, &rec))
    })
    .join()
    .map_err(|_| anyhow!("the restore thread panicked"))
    .and_then(|r| r);
    Some(match r {
        Ok((msg, all)) => {
            if all {
                let _ = std::fs::remove_file(&path);
            }
            format!("{msg} (left by an earlier run)")
        }
        Err(e) => format!("could not restore the default playback device yet: {e:#}"),
    })
}

/// The process-wide switch, shared by the audio sessions.
#[derive(Debug)]
struct Held {
    rec: Record,
    sink: DeviceInfo,
    users: u32,
    /// Bumped by every acquire and release, so a stale grace timer does nothing.
    generation: u64,
}

static HELD: Mutex<Option<Held>> = Mutex::new(None);

/// The sink endpoint a running stream has switched to right now (its route record's sender still runs, or this
/// process holds the switch). An install's default-device recovery must not move such a role away.
pub fn live_route_sink() -> Option<String> {
    live_route_sink_with(held().as_ref())
}

/// [`live_route_sink`] when the caller knows whether this process holds the switch (it holds the lock).
fn live_route_sink_with(here: Option<&Held>) -> Option<String> {
    let rec = Record::parse(&std::fs::read_to_string(route_path()?).ok()?)?;
    let me = std::process::id();
    let live = if rec.pid == me { here.is_some() } else { crate::winutil::process_alive(rec.pid) };
    live.then_some(rec.sink)
}

fn held() -> std::sync::MutexGuard<'static, Option<Held>> {
    HELD.lock().expect("audio route lock poisoned")
}

/// One audio session's use of the virtual sink. Dropping it releases the switch (after [`GRACE`] when it was the
/// last user).
#[derive(Debug)]
pub struct Route {
    pub sink: DeviceInfo,
    /// Where "Also play on PC" plays: the console default before the switch.
    pub pc_device: Option<String>,
}

/// Use the virtual sink chosen by `choice`: make it the default (or join the switch another session made).
/// `Ok(None)`: no such device, so the caller falls back to plain loopback.
///
/// # Errors
///
/// A sink was found but the switch failed (the caller falls back too).
pub fn acquire(choice: &SinkChoice) -> Result<Option<Route>> {
    if *choice == SinkChoice::Off {
        return Ok(None);
    }
    let mut g = held();
    if let Some(h) = g.as_mut() {
        h.users += 1;
        h.generation += 1;
        return Ok(Some(Route { sink: h.sink.clone(), pc_device: h.rec.pc_device().map(str::to_string) }));
    }
    let mut sys = Windows::new()?;
    let devices = sys.devices()?;
    let Some(sink) = pick(&devices, choice).cloned() else {
        if let SinkChoice::Named(n) = choice {
            eprintln!("[audio] no playback device matches --audio-sink {n:?}; using plain loopback");
        }
        return Ok(None);
    };
    let path = route_path().context("LOCALAPPDATA is not set")?;
    // A sender from before the rename (SlipStream) that still streams keeps its record in the old folder until it
    // is gone (see `crate::paths`): its route counts as live here too.
    if let Some(legacy) = crate::paths::legacy_local_dir() {
        refuse_live_route(&mut sys, &legacy.join("audio-route.json"), std::process::id())?;
    }
    let rec = engage(&mut sys, &sink, &path, std::process::id())?;
    eprintln!("[audio] virtual sink: {} is the default playback device while streaming", sink.name);
    let route = Route { sink: sink.clone(), pc_device: rec.pc_device().map(str::to_string) };
    *g = Some(Held { rec, sink, users: 1, generation: 0 });
    Ok(Some(route))
}

impl Drop for Route {
    fn drop(&mut self) {
        let generation = {
            let mut g = held();
            let Some(h) = g.as_mut() else { return };
            h.users = h.users.saturating_sub(1);
            h.generation += 1;
            if h.users > 0 {
                return;
            }
            h.generation
        };
        std::thread::spawn(move || {
            std::thread::sleep(GRACE);
            let mut g = held();
            if g.as_ref().is_some_and(|h| h.users == 0 && h.generation == generation) {
                if let Some(h) = g.take() {
                    release(&h.rec);
                    after_release(g.as_ref());
                }
            }
        });
    }
}

/// The stream let go of the sink: an install whose default-device recovery waited for it can finish now. Call
/// with the switch's lock held and the switch released, so no new stream can take the sink while it runs (the
/// next one waits for the lock, then switches as usual).
fn after_release(here: Option<&Held>) {
    let owned: Vec<String> = live_route_sink_with(here).into_iter().collect();
    if let Some(m) = super::driver::recover_stale_locked(&owned) {
        eprintln!("[audio] {m}");
    }
}

fn release(rec: &Record) {
    let r = Windows::new().map(|mut sys| restore(&mut sys, rec));
    match r {
        Ok((msg, all)) => {
            eprintln!("[audio] {msg}");
            if all {
                if let Some(p) = route_path() {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Err(e) => eprintln!("[audio] could not restore the default playback device: {e:#}"),
    }
}

/// Put the defaults back, and undo a parked mute, now (the sender is exiting). Held by `main`; dropping it
/// releases.
#[derive(Debug)]
pub struct ExitGuard;

impl Drop for ExitGuard {
    fn drop(&mut self) {
        super::tv_only::release_parked();
        let mut g = held();
        if let Some(h) = g.take() {
            release(&h.rec);
            after_release(g.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn dev(id: &str, name: &str, adapter: &str) -> DeviceInfo {
        DeviceInfo { id: id.into(), name: name.into(), adapter: adapter.into() }
    }

    fn devices() -> Vec<DeviceInfo> {
        vec![
            dev("{spk}", "Speakers (Realtek(R) Audio)", "Realtek(R) Audio"),
            dev("{cable}", "CABLE Input (VB-Audio Virtual Cable)", "VB-Audio Virtual Cable"),
            dev("{steam}", "Speakers (Steam Streaming Speakers)", "Steam Streaming Speakers"),
        ]
    }

    #[test]
    fn auto_takes_the_known_sinks_in_order() {
        let d = devices();
        assert_eq!(pick(&d, &SinkChoice::Auto).map(|x| x.id.as_str()), Some("{steam}"), "Steam first");
        let no_steam: Vec<_> = d.iter().filter(|x| x.id != "{steam}").cloned().collect();
        assert_eq!(pick(&no_steam, &SinkChoice::Auto).map(|x| x.id.as_str()), Some("{cable}"));
        let vad = vec![dev("{v}", "Speakers (Virtual Audio Driver)", "Virtual Audio Driver by MTT")];
        assert_eq!(pick(&vad, &SinkChoice::Auto).map(|x| x.id.as_str()), Some("{v}"));
        let ours = vec![dev("{s}", "Speakers (TunnelRez TV)", "TunnelRez TV Audio")];
        assert_eq!(pick(&ours, &SinkChoice::Auto).map(|x| x.id.as_str()), Some("{s}"));
        assert_eq!(pick(&d[..1], &SinkChoice::Auto), None, "only real speakers: no sink");
        assert_eq!(pick(&d, &SinkChoice::Off), None);
    }

    #[test]
    fn a_named_sink_matches_id_then_name_then_part() {
        let d = devices();
        let p = |n: &str| pick(&d, &SinkChoice::parse(n)).map(|x| x.id.clone());
        assert_eq!(p("{CABLE}").as_deref(), Some("{cable}"), "endpoint ID");
        assert_eq!(p("speakers (realtek(r) audio)").as_deref(), Some("{spk}"), "exact name");
        assert_eq!(p("VB-Audio").as_deref(), Some("{cable}"), "part of a name");
        assert_eq!(p("nothing like it"), None);
        assert_eq!(SinkChoice::parse(""), SinkChoice::Auto);
        assert_eq!(SinkChoice::parse("off"), SinkChoice::Off);
    }

    /// A fake Windows: defaults per role, formats per device, and switches to refuse.
    #[derive(Default)]
    struct Fake {
        defaults: HashMap<&'static str, String>,
        formats: HashMap<String, Vec<u8>>,
        refuse_default: Option<Role>,
        refuse_format: bool,
        calls: Vec<String>,
        /// Sender pids that still run.
        alive: Vec<u32>,
    }

    impl System for Fake {
        fn devices(&mut self) -> Result<Vec<DeviceInfo>> {
            Ok(devices())
        }
        fn default_id(&mut self, role: Role) -> Option<String> {
            self.defaults.get(role.key()).cloned()
        }
        fn set_default(&mut self, id: &str, role: Role) -> Result<()> {
            if self.refuse_default == Some(role) {
                return Err(anyhow!("refused"));
            }
            self.calls.push(format!("{}={id}", role.key()));
            self.defaults.insert(role.key(), id.to_string());
            Ok(())
        }
        fn device_format(&mut self, id: &str) -> Result<Vec<u8>> {
            self.formats.get(id).cloned().ok_or_else(|| anyhow!("no format"))
        }
        fn set_device_format(&mut self, id: &str, format: &[u8]) -> Result<()> {
            if self.refuse_format {
                return Err(anyhow!("refused"));
            }
            self.formats.insert(id.to_string(), format.to_vec());
            Ok(())
        }
        fn process_alive(&mut self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
    }

    fn fake() -> Fake {
        let mut f = Fake::default();
        for r in Role::ALL {
            f.defaults.insert(r.key(), "{spk}".into());
        }
        f.defaults.insert("communications", "{headset}".into());
        f.formats.insert("{cable}".into(), vec![1, 2, 3]);
        f
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("tunnelrez-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn switch_and_restore_all_three_roles() {
        let (mut f, path) = (fake(), temp("route-roundtrip"));
        let cable = devices()[1].clone();
        let rec = engage(&mut f, &cable, &path, 42).unwrap();
        for r in Role::ALL {
            assert_eq!(f.default_id(r).as_deref(), Some("{cable}"), "{r:?} switched");
        }
        assert_eq!(f.formats["{cable}"], policy::stereo_48k_s16(), "sink set to 48 kHz s16 stereo");
        assert_eq!(rec.pc_device(), Some("{spk}"), "echo goes to the old console default");
        let on_disk = Record::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk, rec, "the record is on disk (and round-trips) before the switch is used");
        let (msg, all) = restore(&mut f, &rec);
        assert!(all, "{msg}");
        assert_eq!(f.default_id(Role::Console).as_deref(), Some("{spk}"));
        assert_eq!(f.default_id(Role::Communications).as_deref(), Some("{headset}"), "each role gets its own back");
        assert_eq!(f.formats["{cable}"], vec![1, 2, 3], "old sink format back");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_role_the_user_changed_since_is_left_alone() {
        let (mut f, path) = (fake(), temp("route-user"));
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        f.defaults.insert("multimedia", "{hdmi}".into()); // the user picked another device mid-stream
        let (msg, all) = restore(&mut f, &rec);
        assert!(all && msg.contains("1 left as the user changed"), "{msg}");
        assert_eq!(f.default_id(Role::Multimedia).as_deref(), Some("{hdmi}"));
        assert_eq!(f.default_id(Role::Console).as_deref(), Some("{spk}"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_sink_that_was_already_default_is_not_touched() {
        let mut f = fake();
        for r in Role::ALL {
            f.defaults.insert(r.key(), "{cable}".into());
        }
        let path = temp("route-already");
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        assert!(f.calls.is_empty(), "no default changed: {:?}", f.calls);
        assert_eq!(rec.pc_device(), None, "no PC device to echo to");
        let (_, all) = restore(&mut f, &rec);
        assert!(all && f.calls.is_empty(), "nothing to undo: {:?}", f.calls);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_live_route_in_the_old_folder_blocks_a_new_one() {
        let mut f = fake();
        let legacy = temp("route-legacy");
        // A SlipStream sender (pid 7) streams through the Steam speakers; its record stays in the old folder.
        let rec = Record {
            sink: "{steam}".into(),
            sink_name: "Speakers (Steam Streaming Speakers)".into(),
            before: Role::ALL.iter().map(|r| (*r, "{spk}".to_string())).collect(),
            format: None,
            pid: 7,
        };
        std::fs::write(&legacy, rec.to_json()).unwrap();
        f.alive.push(7);
        let e = refuse_live_route(&mut f, &legacy, 8).expect_err("pid 7 still streams");
        assert!(e.to_string().contains("pid 7"), "{e}");
        // Once it is gone, a new route may start; our own record never blocks us.
        f.alive.clear();
        assert!(refuse_live_route(&mut f, &legacy, 8).is_ok());
        f.alive.push(7);
        assert!(refuse_live_route(&mut f, &legacy, 7).is_ok());
        assert!(refuse_live_route(&mut f, &temp("route-none"), 8).is_ok(), "no record");
        let _ = std::fs::remove_file(&legacy);
    }

    #[test]
    fn a_refused_switch_puts_back_what_changed() {
        let mut f = fake();
        f.refuse_default = Some(Role::Communications);
        let path = temp("route-refused");
        assert!(engage(&mut f, &devices()[1], &path, 1).is_err());
        assert_eq!(f.default_id(Role::Console).as_deref(), Some("{spk}"), "console put back");
        assert_eq!(f.default_id(Role::Multimedia).as_deref(), Some("{spk}"), "multimedia put back");
        assert!(!path.exists(), "nothing left to recover");
    }

    #[test]
    fn a_failed_rollback_keeps_the_record() {
        // Console and multimedia switch; communications is refused; then putting console back fails too.
        struct Stuck<'a>(&'a mut Fake, u32);
        impl System for Stuck<'_> {
            fn devices(&mut self) -> Result<Vec<DeviceInfo>> {
                self.0.devices()
            }
            fn default_id(&mut self, role: Role) -> Option<String> {
                self.0.default_id(role)
            }
            fn set_default(&mut self, id: &str, role: Role) -> Result<()> {
                self.1 += 1;
                if self.1 > 3 {
                    return Err(anyhow!("old device gone"));
                }
                self.0.set_default(id, role)
            }
            fn device_format(&mut self, id: &str) -> Result<Vec<u8>> {
                self.0.device_format(id)
            }
            fn set_device_format(&mut self, id: &str, format: &[u8]) -> Result<()> {
                self.0.set_device_format(id, format)
            }
            fn process_alive(&mut self, pid: u32) -> bool {
                self.0.process_alive(pid)
            }
        }
        let mut f = fake();
        f.refuse_default = Some(Role::Communications);
        let path = temp("route-rollback");
        let e = engage(&mut Stuck(&mut f, 0), &devices()[1], &path, 1).expect_err("switch refused");
        assert!(format!("{e:#}").contains("rollback was incomplete"), "{e:#}");
        assert!(path.exists(), "the record stays while a role still points at the sink");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_new_switch_keeps_an_unfinished_restore() {
        let (mut f, path) = (fake(), temp("route-pending"));
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        // The restore failed (speakers unplugged): the roles still point at the sink and the record stays.
        f.refuse_default = Some(Role::Console);
        let (_, all) = restore(&mut f, &rec);
        assert!(!all);
        f.refuse_default = None;
        // The next stream switches again: the record must still name the original devices and format.
        let again = engage(&mut f, &devices()[1], &path, 2).unwrap();
        assert_eq!(again.pc_device(), Some("{spk}"), "not the sink itself");
        assert_eq!(again.format, Some(vec![1, 2, 3]), "the original format, not the target");
        let (_, all) = restore(&mut f, &again);
        assert!(all);
        assert_eq!(f.default_id(Role::Console).as_deref(), Some("{spk}"));
        assert_eq!(f.formats["{cable}"], vec![1, 2, 3]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unfinished_restore_of_another_sink_blocks_a_switch() {
        let (mut f, path) = (fake(), temp("route-other"));
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        f.refuse_default = Some(Role::Console);
        assert!(!restore(&mut f, &rec).1);
        // Still refused: switching to the Steam sink now would overwrite the CABLE record.
        let e = engage(&mut f, &devices()[2], &path, 2).expect_err("blocked");
        assert!(format!("{e:#}").contains("not finished"), "{e:#}");
        let kept = Record::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((kept.sink.as_str(), kept.format.clone()), ("{cable}", Some(vec![1, 2, 3])));
        // Windows takes it now: the old restore finishes first, then the new switch goes ahead.
        f.refuse_default = None;
        let steam = engage(&mut f, &devices()[2], &path, 3).unwrap();
        assert_eq!(steam.pc_device(), Some("{spk}"));
        assert_eq!(f.formats["{cable}"], vec![1, 2, 3], "CABLE got its format back");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pending_format_survives_a_refused_reapply() {
        let (mut f, path) = (fake(), temp("route-reapply"));
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        f.refuse_format = true; // the format can neither go back nor be set again
        f.refuse_default = Some(Role::Console);
        assert!(!restore(&mut f, &rec).1);
        f.refuse_default = None;
        let again = engage(&mut f, &devices()[1], &path, 2).unwrap();
        assert_eq!(again.format, Some(vec![1, 2, 3]), "the original format is still recorded");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_live_sender_route_is_left_alone() {
        let (mut f, path) = (fake(), temp("route-live"));
        let first = engage(&mut f, &devices()[1], &path, 10).unwrap();
        f.alive.push(10);
        let calls = f.calls.len();
        let e = engage(&mut f, &devices()[1], &path, 20).expect_err("sender 10 still streams");
        assert!(format!("{e:#}").contains("pid 10"), "{e:#}");
        assert_eq!(f.calls.len(), calls, "no default changed");
        assert_eq!(f.formats["{cable}"], policy::stereo_48k_s16(), "its format untouched");
        assert_eq!(Record::parse(&std::fs::read_to_string(&path).unwrap()), Some(first), "its record untouched");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failed_format_restore_keeps_the_record() {
        let (mut f, path) = (fake(), temp("route-format-back"));
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        f.refuse_format = true;
        let (msg, all) = restore(&mut f, &rec);
        assert!(!all && msg.contains("format"), "{msg}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_refused_format_still_switches_and_converts() {
        let mut f = fake();
        f.refuse_format = true;
        let path = temp("route-format");
        let rec = engage(&mut f, &devices()[1], &path, 1).unwrap();
        assert_eq!(rec.format, None, "no format to put back");
        assert_eq!(f.default_id(Role::Console).as_deref(), Some("{cable}"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn records_parse_and_bad_ones_do_not() {
        let rec = Record {
            sink: "{c}".into(),
            sink_name: "CABLE".into(),
            before: vec![(Role::Console, "{a}".into()), (Role::Communications, "{b}".into())],
            format: Some(vec![0xfe, 0xff, 0]),
            pid: 7,
        };
        assert_eq!(Record::parse(&rec.to_json()), Some(rec));
        assert_eq!(Record::parse("garbage"), None);
        assert_eq!(Record::parse(r#"{"sink":"x"}"#), None);
        assert_eq!(unhex("0g"), None);
    }
}
