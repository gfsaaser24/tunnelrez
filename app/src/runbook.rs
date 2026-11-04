//! "Add to run book": hand a finished log (JSON) to `tools/runbook/add-run.ps1` of a `TunnelRez` checkout, which
//! writes `docs/runbook/runs/<id>/` and rebuilds the run book table.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::platform;

/// The importer, relative to a checkout.
pub(crate) const SCRIPT: &str = r"tools\runbook\add-run.ps1";
/// How many folders above the app and the sender are searched for a checkout.
const DEPTH: usize = 6;

/// The importer: `TUNNELREZ_REPO`, else the first checkout above the app, else above the sender.
pub(crate) fn find_script(sender: Option<&Path>) -> Option<PathBuf> {
    let repo = std::env::var_os("TUNNELREZ_REPO").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    candidates(repo.as_deref(), exe.as_deref(), sender).into_iter().find(|p| p.is_file())
}

/// Where to look, best first.
fn candidates(repo: Option<&Path>, exe: Option<&Path>, sender: Option<&Path>) -> Vec<PathBuf> {
    let above = |p: Option<&Path>| {
        p.and_then(Path::parent)
            .into_iter()
            .flat_map(|d| d.ancestors().take(DEPTH))
            .map(|a| a.join(SCRIPT))
            .collect::<Vec<_>>()
    };
    repo.map(|r| r.join(SCRIPT)).into_iter().chain(above(exe)).chain(above(sender)).collect()
}

/// The longest "content" note handed to the importer, in characters.
const CONTENT_MAX: usize = 200;

/// What the importer did with a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Added {
    /// The run's id (`2026-10-26-04`) when the importer said it.
    pub(crate) id: Option<String>,
    /// The panel's words.
    pub(crate) message: String,
}

/// Run the importer on `log`, with an optional `content` note (what was on screen). A log that is already in
/// the run book is not added again (the importer says which run has it).
///
/// # Errors
/// When `PowerShell` cannot start, or the importer fails (its first error line).
pub(crate) fn add(script: &Path, log: &Path, content: &str) -> Result<Added> {
    // PowerShell 7 when installed, else the Windows PowerShell every PC has.
    let shell = platform::which("pwsh.exe").unwrap_or_else(|| PathBuf::from("powershell.exe"));
    let out = platform::hide_console(&mut Command::new(shell))
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .arg("-FromLog")
        .arg(log)
        .args(content_args(content))
        .stdin(Stdio::null())
        .output()
        .context("start PowerShell")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{}", error_line(&stderr).or_else(|| error_line(&stdout)).unwrap_or("the run book script failed"));
    }
    Ok(added(&stdout))
}

/// `-Content <note>`: one line, no IP addresses, at most [`CONTENT_MAX`] characters; nothing when empty.
fn content_args(content: &str) -> Vec<String> {
    let one_line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let note: String = crate::logger::redact_ips(&one_line).chars().take(CONTENT_MAX).collect();
    if note.is_empty() {
        Vec::new()
    } else {
        vec!["-Content".into(), note]
    }
}

/// The importer's last words -> what happened. `run <id> saved in ... [(marked: short: 21 s)]` is a new run (marked
/// when it is short or what was on screen changed); `already <id> ...` means the same log is in the run book already.
fn added(stdout: &str) -> Added {
    let after = |prefix: &str| {
        stdout.lines().find_map(|l| l.trim().strip_prefix(prefix)?.split_whitespace().next().map(str::to_string))
    };
    if let Some(id) = after("already ") {
        let message = format!("Already in the run book as {id} (this log). Not added again.");
        return Added { id: Some(id), message };
    }
    let marked = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("run ")?.split_once("(marked: ")?.1.strip_suffix(')').map(str::to_string));
    match after("run ") {
        Some(id) => {
            let mark = marked.map(|m| format!(", marked {m}")).unwrap_or_default();
            Added { message: format!("Added as {id}{mark}. Commit docs/runbook to keep it."), id: Some(id) }
        }
        None => Added { id: None, message: "Added to the run book. Commit docs/runbook to keep it.".into() },
    }
}

/// The first line of a `PowerShell` error, without the `<script> : ` it starts with.
fn error_line(text: &str) -> Option<&str> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.rsplit_once(".ps1 : ").map_or(line, |(_, msg)| msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_looked_for_above_the_app_and_the_sender() {
        let c = candidates(
            Some(Path::new(r"D:\repo")),
            Some(Path::new(r"C:\code\tunnelrez\app\target\release\tunnelrez.exe")),
            Some(Path::new(r"C:\code\other\sender\target\release\tunnelrez-send.exe")),
        );
        assert_eq!(c[0], Path::new(r"D:\repo\tools\runbook\add-run.ps1"), "TUNNELREZ_REPO first");
        assert!(c.contains(&PathBuf::from(r"C:\code\tunnelrez\tools\runbook\add-run.ps1")), "{c:?}");
        assert!(c.contains(&PathBuf::from(r"C:\code\other\tools\runbook\add-run.ps1")), "{c:?}");
        let app = c.iter().position(|p| p.starts_with(r"C:\code\tunnelrez")).unwrap();
        let sender = c.iter().position(|p| p.starts_with(r"C:\code\other")).unwrap();
        assert!(app < sender, "the app's checkout before the sender's");
        assert!(candidates(None, None, None).is_empty());
    }

    #[test]
    fn messages() {
        let a = added("run 2026-10-26-04 saved in C:\\x\\docs\\runbook\\runs\\2026-10-26-04\n");
        assert_eq!(a.id.as_deref(), Some("2026-10-26-04"));
        assert_eq!(a.message, "Added as 2026-10-26-04. Commit docs/runbook to keep it.");
        let a =
            added("run 2026-10-26-13 saved in C:\\x\\runs\\2026-10-26-13 (marked: short: 21 s, content changed)\r\n");
        assert_eq!(a.id.as_deref(), Some("2026-10-26-13"));
        assert_eq!(
            a.message,
            "Added as 2026-10-26-13, marked short: 21 s, content changed. Commit docs/runbook to keep it."
        );
        let a = added("already 2026-10-26-05 in the run book (the same log, 2026-10-26_20-22-40.runbook.json)\n");
        assert_eq!(a.id.as_deref(), Some("2026-10-26-05"));
        assert_eq!(a.message, "Already in the run book as 2026-10-26-05 (this log). Not added again.");
        let a = added("");
        assert_eq!(a.id, None);
        assert!(a.message.starts_with("Added to the run book"));
        let err =
            "C:\\code\\tunnelrez\\tools\\runbook\\add-run.ps1 : no TV samples in the log (the TV was not connected)\n\
                   At line:1 char:1\n+ ...";
        assert_eq!(error_line(err), Some("no TV samples in the log (the TV was not connected)"));
        assert_eq!(error_line("\n  plain failure\n"), Some("plain failure"));
        assert_eq!(error_line(""), None);
    }

    #[test]
    fn content_note() {
        assert!(content_args("  ").is_empty());
        assert_eq!(content_args(" motion60 page,\n full screen "), ["-Content", "motion60 page, full screen"]);
        assert_eq!(content_args("video from 192.0.2.5:8080"), ["-Content", "video from <ip>"]);
        assert_eq!(content_args(&"x".repeat(500))[1].chars().count(), CONTENT_MAX);
    }
}
