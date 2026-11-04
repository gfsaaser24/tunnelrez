//! "Play on TV only": mute the PC's default playback endpoint while audio streams to the TV, and put the mute
//! back after. WASAPI loopback taps the mix before the endpoint mute, so the TV still gets the sound (checked
//! with `tunnelrez-send audio-probe`).
//!
//! A restore file lists every endpoint still to unmute, written before the mute is set and cleared after the
//! unmute worked, so a killed sender (or an unmute that failed) is fixed at a later start ([`recover_stale`]).

use super::sink::GRACE;
use super::wasapi::{Com, Endpoint, Mute};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::Mutex;

/// `%LOCALAPPDATA%\TunnelRez\<name>`: where the audio restore records live (see [`crate::paths`]).
pub(super) fn state_path(name: &str) -> Option<PathBuf> {
    Some(crate::paths::local_dir()?.join(name))
}

/// `%LOCALAPPDATA%\TunnelRez\audio-restore.json`.
fn restore_path() -> Option<PathBuf> {
    state_path("audio-restore.json")
}

/// The endpoints a restore file says to unmute: `{"unmute":[ids]}`, or the first format `{"device":id,
/// "mute":false}`. None when the file is unreadable.
fn parse_pending(s: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(s).ok()?;
    if let Some(list) = v.get("unmute").and_then(Value::as_array) {
        return Some(list.iter().filter_map(Value::as_str).map(str::to_string).collect());
    }
    let device = v.get("device")?.as_str()?;
    Some(if v.get("mute")? == &Value::Bool(false) { vec![device.to_string()] } else { Vec::new() })
}

/// The pending list in `path` (empty when there is no file or it is unreadable).
fn read_pending(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).ok().and_then(|s| parse_pending(&s)).unwrap_or_default()
}

/// Write the pending list; an empty list removes the file. The new list goes to a temporary file first and
/// replaces the old one in one rename, so a failed or interrupted write leaves the old list intact (it may hold
/// other devices still to unmute).
fn write_pending(path: &Path, pending: &[String]) -> std::io::Result<()> {
    if pending.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    write_atomic(path, &json!({ "unmute": pending, "pid": std::process::id() }).to_string())
}

/// The record belongs to another sender that still runs (its stream still needs the mute).
fn owned_by_live(text: &str, me: u32, alive: impl Fn(u32) -> bool) -> Option<u32> {
    let v: Value = serde_json::from_str(text).ok()?;
    let pid = u32::try_from(v.get("pid")?.as_u64()?).ok()?;
    (pid != me && alive(pid)).then_some(pid)
}

/// Write `text` to `path` through a temporary file and one rename, so a failed or interrupted write leaves the
/// old content intact.
pub(super) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        use std::io::Write;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Windows: MoveFileExW with MOVEFILE_REPLACE_EXISTING, an atomic replace on the same volume.
    std::fs::rename(&tmp, path)
}

/// Add `device` to the pending unmutes (before muting it). Other pending entries stay.
fn pending_add(path: &Path, device: &str) -> std::io::Result<()> {
    let mut p = read_pending(path);
    if !p.iter().any(|d| d == device) {
        p.push(device.to_string());
    }
    write_pending(path, &p)
}

/// `device` was unmuted: take it off the list. Other pending entries stay.
fn pending_remove(path: &Path, device: &str) -> std::io::Result<()> {
    let mut p = read_pending(path);
    p.retain(|d| d != device);
    write_pending(path, &p)
}

/// The PC endpoint muted for "Play on TV only". Dropping it unmutes the endpoint (if it was unmuted before) and
/// takes it off the restore list. Create and drop it on a thread with COM initialized.
#[derive(Debug)]
pub struct TvOnly {
    /// Endpoint ID of the muted device.
    id: String,
    /// What the user had: only an endpoint that was unmuted is unmuted again.
    before: Mute,
}

/// A mute kept between sessions (see [`park`]), with the generation that parked it.
static PARKED: Mutex<Option<(TvOnly, u64)>> = Mutex::new(None);
static PARK_GENERATION: AtomicU64 = AtomicU64::new(0);

fn parked() -> std::sync::MutexGuard<'static, Option<(TvOnly, u64)>> {
    PARKED.lock().expect("parked mute lock poisoned")
}

/// Keep a session's mute for [`GRACE`] after the session ends: an encoder fallback or a reconnect starts the
/// next session within a second, and unmuting in between would let the PC speakers play. The next session takes
/// it over ([`TvOnly::engage`]); otherwise it is undone after the grace period (or by [`release_parked`]).
///
/// The unmute runs with the parked slot locked, and [`TvOnly::engage`] holds the same lock while it reads and sets
/// the mute, so an expiring mute can never be taken for the user's own mute by a session starting at that moment.
pub fn park(t: TvOnly) {
    let generation = PARK_GENERATION.fetch_add(1, SeqCst) + 1;
    {
        let mut p = parked();
        let old = p.replace((t, generation));
        drop(old); // a different, older mute: undo it now
    }
    std::thread::spawn(move || {
        std::thread::sleep(GRACE);
        let _com = Com::init();
        let mut p = parked();
        if p.as_ref().is_some_and(|(_, g)| *g == generation) {
            drop(p.take()); // unmutes, still under the lock
        }
    });
}

/// Undo a parked mute now (the sender is exiting).
pub fn release_parked() {
    let _com = Com::init();
    let mut p = parked();
    drop(p.take());
}

impl TvOnly {
    /// Mute the current default playback endpoint (or take over the mute a just-ended session parked for it).
    ///
    /// # Errors
    ///
    /// No playback device, the restore file cannot be written, or the mute cannot be set.
    pub fn engage() -> Result<TvOnly> {
        let ep = Endpoint::default_render()?;
        // Held until the mute is read and set: a grace timer cannot unmute in between (see [`park`]).
        let mut p = parked();
        if let Some((t, _)) = p.take() {
            if t.id == ep.id {
                return Ok(t);
            }
            drop(t); // another device: unmute it
        }
        let before = ep.mute()?;
        if before == Mute::Off {
            let path = restore_path().context("LOCALAPPDATA is not set")?;
            pending_add(&path, &ep.id).with_context(|| format!("write {}", path.display()))?;
            ep.set_mute(Mute::On)?;
        }
        drop(p);
        Ok(TvOnly { id: ep.id, before })
    }

    /// The muted endpoint's ID.
    pub fn device(&self) -> &str {
        &self.id
    }

    /// The PC speakers were already muted by the user (nothing to undo).
    pub fn was_muted(&self) -> bool {
        self.before == Mute::On
    }
}

impl Drop for TvOnly {
    fn drop(&mut self) {
        if self.before == Mute::Off {
            if let Err(e) = Endpoint::by_id(&self.id).and_then(|ep| ep.set_mute(Mute::Off)) {
                eprintln!("[audio] could not unmute the PC speakers: {e:#}");
                return; // stays on the restore list: a later start tries again
            }
            if let Some(p) = restore_path() {
                let _ = pending_remove(&p, &self.id);
            }
        }
    }
}

/// Undo mutes an earlier run left behind (restore file present). Returns a log line when there was a file.
/// The unmutes run on their own short thread, so the caller's thread needs no COM.
pub fn recover_stale() -> Option<String> {
    let path = restore_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    if let Some(pid) = owned_by_live(&text, std::process::id(), crate::winutil::process_alive) {
        return Some(format!("PC speakers stay muted: the sender that muted them (pid {pid}) still streams"));
    }
    recover_file(&path, |device| {
        let device = device.to_string();
        std::thread::spawn(move || -> Result<()> {
            let _com = Com::init()?;
            Endpoint::by_id(&device)?.set_mute(Mute::Off)
        })
        .join()
        .map_err(|_| anyhow!("the unmute thread panicked"))?
    })
}

/// Apply the restore file at `path` with `unmute`, one endpoint at a time. Endpoints that were unmuted leave the
/// list; the ones that failed (not plugged in right now) stay for the next start. An unreadable file is removed.
fn recover_file(path: &Path, mut unmute: impl FnMut(&str) -> Result<()>) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let Some(pending) = parse_pending(&text) else {
        let _ = std::fs::remove_file(path);
        return Some("ignored an unreadable audio restore file".into());
    };
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    for d in &pending {
        if let Err(e) = unmute(d) {
            errors.push(format!("{e:#}"));
            failed.push(d.clone());
        }
    }
    let _ = write_pending(path, &failed);
    let done = pending.len() - failed.len();
    Some(if failed.is_empty() {
        format!("unmuted {done} PC playback device(s) left muted by an earlier run (Play on TV only)")
    } else {
        format!(
            "unmuted {done}, could not unmute {} PC playback device(s) yet ({}); trying again next start",
            failed.len(),
            errors.join("; ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tunnelrez-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_live_sender_keeps_its_mute() {
        let rec = r#"{"unmute":["A"],"pid":42}"#;
        assert_eq!(owned_by_live(rec, 7, |p| p == 42), Some(42), "another sender still streams");
        assert_eq!(owned_by_live(rec, 7, |_| false), None, "that sender is gone: recover");
        assert_eq!(owned_by_live(rec, 42, |_| true), None, "our own record");
        assert_eq!(owned_by_live(r#"{"unmute":["A"]}"#, 7, |_| true), None, "an old record without a pid");
        let path = temp("restore-pid");
        pending_add(&path, "A").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(owned_by_live(&text, 0, |p| p == std::process::id()), Some(std::process::id()), "pid recorded");
        write_pending(&path, &[]).unwrap();
    }

    #[test]
    fn restore_file_formats() {
        assert_eq!(parse_pending(r#"{"unmute":["A","B"]}"#), Some(vec!["A".into(), "B".into()]));
        assert_eq!(parse_pending(r#"{"device":"A","mute":false}"#), Some(vec!["A".into()]), "first format");
        assert_eq!(parse_pending(r#"{"device":"A","mute":true}"#), Some(Vec::new()), "was muted: nothing to undo");
        assert_eq!(parse_pending("not json"), None);
        assert_eq!(parse_pending(r#"{"mute":false}"#), None);
    }

    #[test]
    fn a_pending_unmute_survives_muting_another_device() {
        let path = temp("restore-handoff");
        // A muted, then its unmute fails (A unplugged): A stays on the list.
        pending_add(&path, "A").unwrap();
        // The stream moves to B: B muted, later unmuted.
        pending_add(&path, "B").unwrap();
        assert_eq!(read_pending(&path), ["A", "B"]);
        pending_remove(&path, "B").unwrap();
        assert_eq!(read_pending(&path), ["A"], "B's unmute does not drop A's record");
        // A is back at the next start: recovery unmutes it and the file goes.
        let mut seen = Vec::new();
        let msg = recover_file(&path, |d| {
            seen.push(d.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, ["A"]);
        assert!(msg.starts_with("unmuted 1"), "{msg}");
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_write_leaves_the_old_list() {
        let path = temp("restore-atomic");
        pending_add(&path, "A").unwrap();
        // The temporary file cannot be created (a directory is in its place): the write fails, A's record stays.
        let tmp = path.with_extension("json.tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(pending_add(&path, "B").is_err());
        assert_eq!(read_pending(&path), ["A"], "the old list is intact after a failed write");
        std::fs::remove_dir(&tmp).unwrap();
        pending_add(&path, "B").unwrap();
        assert_eq!(read_pending(&path), ["A", "B"]);
        assert!(!tmp.exists(), "no temporary file left");
        write_pending(&path, &[]).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_recovery_keeps_the_record_for_the_next_start() {
        let path = temp("restore-retry");
        pending_add(&path, "A").unwrap();
        pending_add(&path, "B").unwrap();
        let msg = recover_file(&path, |d| if d == "A" { Err(anyhow!("device not present")) } else { Ok(()) }).unwrap();
        assert!(msg.contains("trying again"), "{msg}");
        assert_eq!(read_pending(&path), ["A"], "only the failed one stays");
        let msg = recover_file(&path, |_| Ok(())).unwrap();
        assert!(msg.starts_with("unmuted 1"), "{msg}");
        assert!(!path.exists(), "removed once everything is unmuted");
        assert_eq!(recover_file(&path, |_| Ok(())), None, "no file: nothing to do");
        std::fs::write(&path, "garbage").unwrap();
        assert!(recover_file(&path, |_| Ok(())).unwrap().contains("unreadable"));
        assert!(!path.exists());
    }
}
