//! Where the sender keeps its state: `%LOCALAPPDATA%\TunnelRez` (encoder cache, TV speaker marker, audio restore
//! records).
//!
//! Before the rename the folder was `%LOCALAPPDATA%\SlipStream`. The first call to [`local_dir`] copies that folder
//! once and leaves it in place. The audio restore records are moved instead of copied, so a restore that a killed
//! older sender left runs exactly once, from the new folder. A record that an older sender writes later (the old
//! app still runs during an upgrade) is moved over the same way at the next start. A record whose writer (its
//! `pid`, or the elevated `installer`) still runs stays where it is until that process is gone: it still updates and
//! clears that file.
//!
//! When the copy fails, this run keeps using the old folder, so nothing is written to a new folder that a later
//! start would take as a finished copy.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

/// The state folder's name under `%LOCALAPPDATA%`.
pub const DIR: &str = "TunnelRez";
/// The folder's name before the rename.
pub const LEGACY_DIR: &str = "SlipStream";
/// Files that hold work still to do (a default device or a mute to put back): moved, never copied.
const RECORDS: [&str; 3] = ["audio-route.json", "audio-restore.json", "install-defaults.json"];

/// What [`migrate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Migration {
    /// No old folder, or the new folder was already there.
    Nothing,
    /// The old folder was copied to the new one.
    Copied,
}

/// `%LOCALAPPDATA%\TunnelRez`, after the one-time copy from `%LOCALAPPDATA%\SlipStream` (the old folder, for this
/// run, when that copy failed). `None` when `LOCALAPPDATA` is not set.
pub fn local_dir() -> Option<PathBuf> {
    static DIR_NOW: OnceLock<PathBuf> = OnceLock::new();
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    Some(
        DIR_NOW
            .get_or_init(|| {
                let (old, new) = (base.join(LEGACY_DIR), base.join(DIR));
                let me = std::process::id();
                match migrate(&old, &new, |pid| pid != me && crate::winutil::process_alive(pid)) {
                    Ok(_) => new,
                    Err(e) => {
                        eprintln!("[warn] could not copy {} to {}: {e}", old.display(), new.display());
                        if new.is_dir() {
                            new
                        } else {
                            old
                        }
                    }
                }
            })
            .clone(),
    )
}

/// `%LOCALAPPDATA%\SlipStream`: read only for the TV speaker marker an older install left.
pub fn legacy_local_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(LEGACY_DIR))
}

/// Copy `old` to `new` when `new` does not exist yet (through a temporary folder, so two senders starting at once
/// never see half a copy), then move every restore record still in `old` that `new` does not have, unless `alive`
/// says its writer still runs.
///
/// # Errors
/// When the copy or a move fails. A failed copy leaves `new` absent, so the next start tries again.
pub fn migrate(old: &Path, new: &Path, alive: impl Fn(u32) -> bool) -> std::io::Result<Migration> {
    if !old.is_dir() {
        return Ok(Migration::Nothing);
    }
    let mut done = Migration::Nothing;
    if !new.exists() {
        let tmp = new.with_file_name(format!("{DIR}.migrating-{}", std::process::id()));
        let copied = copy_tree(old, &tmp).and_then(|()| std::fs::rename(&tmp, new));
        match copied {
            Ok(()) => done = Migration::Copied,
            // Another sender finished first: its copy is as good as ours.
            Err(_) if new.is_dir() => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
    for name in RECORDS {
        let (from, to) = (old.join(name), new.join(name));
        if from.is_file() && !to.exists() && !writer_alive(&from, &alive) {
            std::fs::create_dir_all(new)?;
            match std::fs::rename(&from, &to) {
                // Another sender starting at the same moment moved it first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                r => r?,
            }
        }
    }
    Ok(done)
}

/// The process that writes the record (`pid`), or the elevated installer it waits for (`installer`), still runs.
/// Every record format has `pid`; `install-defaults.json` may add `installer`. An unreadable record has no writer.
fn writer_alive(record: &Path, alive: impl Fn(u32) -> bool) -> bool {
    let Some(v) = std::fs::read_to_string(record).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return false;
    };
    ["pid", "installer"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()))
        .any(alive)
}

/// Copy the folder `from` to `to` (which must not exist), without the restore records.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if !RECORDS.iter().any(|r| entry.file_name() == *r) {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tunnelrez-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn copies_once_and_moves_records() {
        let d = scratch("copy");
        let (old, new) = (d.join(LEGACY_DIR), d.join(DIR));
        std::fs::create_dir_all(old.join("sub")).unwrap();
        std::fs::write(old.join("encoders.json"), "{}").unwrap();
        std::fs::write(old.join("steam-speakers.json"), r#"{"reboot":false}"#).unwrap();
        std::fs::write(old.join("audio-route.json"), "route").unwrap();
        std::fs::write(old.join("sub").join("x.txt"), "x").unwrap();

        assert_eq!(migrate(&old, &new, |_| false).unwrap(), Migration::Copied);
        assert_eq!(std::fs::read_to_string(new.join("encoders.json")).unwrap(), "{}");
        assert!(new.join("steam-speakers.json").is_file(), "the TV speaker marker comes along");
        assert_eq!(std::fs::read_to_string(new.join("sub").join("x.txt")).unwrap(), "x");
        assert_eq!(std::fs::read_to_string(new.join("audio-route.json")).unwrap(), "route");
        assert!(!old.join("audio-route.json").exists(), "a restore record is moved, so it runs once");
        assert!(old.join("encoders.json").is_file(), "the old folder stays");

        // Second start: nothing is copied again, a newer copy is not overwritten.
        std::fs::write(new.join("encoders.json"), "new").unwrap();
        assert_eq!(migrate(&old, &new, |_| false).unwrap(), Migration::Nothing);
        assert_eq!(std::fs::read_to_string(new.join("encoders.json")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_later_record_from_an_old_sender_is_moved() {
        let d = scratch("late");
        let (old, new) = (d.join(LEGACY_DIR), d.join(DIR));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("audio-restore.json"), "late").unwrap();
        assert_eq!(migrate(&old, &new, |_| false).unwrap(), Migration::Nothing);
        assert_eq!(std::fs::read_to_string(new.join("audio-restore.json")).unwrap(), "late");

        // A record already in the new folder wins; the old one is left alone.
        std::fs::write(old.join("audio-restore.json"), "older").unwrap();
        migrate(&old, &new, |_| false).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("audio-restore.json")).unwrap(), "late");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_record_whose_writer_still_runs_stays() {
        let d = scratch("alive");
        let (old, new) = (d.join(LEGACY_DIR), d.join(DIR));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        // The three formats: a route record and a restore file carry `pid`, install defaults also `installer`.
        std::fs::write(old.join("audio-route.json"), r#"{"sink":"s","pid":7}"#).unwrap();
        std::fs::write(old.join("audio-restore.json"), r#"{"unmute":["A"],"pid":8}"#).unwrap();
        std::fs::write(old.join("install-defaults.json"), r#"{"before":{},"pid":1,"installer":9}"#).unwrap();
        migrate(&old, &new, |p| p == 7 || p == 9).unwrap();
        assert!(old.join("audio-route.json").is_file(), "its sender still runs: it stays");
        assert!(old.join("install-defaults.json").is_file(), "its installer still runs: it stays");
        assert!(new.join("audio-restore.json").is_file(), "its sender is gone: moved");
        assert!(!old.join("audio-restore.json").exists());

        // The next start, once they are gone, moves the rest.
        migrate(&old, &new, |_| false).unwrap();
        assert!(new.join("audio-route.json").is_file() && new.join("install-defaults.json").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_old_folder_does_nothing() {
        let d = scratch("none");
        assert_eq!(migrate(&d.join(LEGACY_DIR), &d.join(DIR), |_| false).unwrap(), Migration::Nothing);
        assert!(!d.join(DIR).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
