//! TV checks: is the `TunnelRez` receiver listening, and (if adb is installed) open it on the TV.
//!
//! Never call [`check`] while streaming: the receiver serves one client, and a test connection replaces the stream.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::platform;

/// The receiver's TCP port (PROTOCOL.md).
pub(crate) const PORT: u16 = 7878;
/// adb over TCP, used only to tell "TV on, app closed" from "TV off" and to open the app.
const ADB_PORT: u16 = 5555;
/// The TV app, newest first. `dev.slipstream.tv` is the app from before the rename: it speaks the same protocol,
/// so a TV that still has only that one keeps working until it is replaced (README, "Switch the TV app").
const PACKAGES: [&str; 2] = ["dev.tunnelrez.tv", "dev.slipstream.tv"];
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reach {
    /// The receiver accepted a connection.
    Ready,
    /// The TV is on the network, but the receiver is not listening.
    AppNotOpen,
    /// Nothing answered.
    Unreachable,
    /// The address does not resolve.
    BadAddress,
}

fn addr(host: &str, port: u16) -> Option<SocketAddr> {
    (host, port).to_socket_addrs().ok()?.next()
}

/// Probe the TV. Takes up to ~2.5 s when nothing answers.
pub(crate) fn check(host: &str) -> Reach {
    let host = host.trim();
    let Some(a) = addr(host, PORT) else { return Reach::BadAddress };
    match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
        Ok(s) => {
            let _ = s.shutdown(std::net::Shutdown::Both);
            Reach::Ready
        }
        // A refusal means the TV itself answered: it is on, the app is not listening.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Reach::AppNotOpen,
        Err(_) => match TcpStream::connect_timeout(&SocketAddr::new(a.ip(), ADB_PORT), Duration::from_millis(800)) {
            Ok(_) => Reach::AppNotOpen,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Reach::AppNotOpen,
            Err(_) => Reach::Unreachable,
        },
    }
}

/// adb on PATH, if any.
pub(crate) fn adb() -> Option<PathBuf> {
    platform::which("adb.exe")
}

fn run_adb(adb: &PathBuf, args: &[&str]) -> Result<String> {
    let out = platform::hide_console(&mut Command::new(adb))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run adb {}", args.join(" ")))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        bail!("adb {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

/// Wake the TV and start the `TunnelRez` app on it over adb (the old `SlipStream` app when only that one is there).
///
/// # Errors
/// When adb is missing, cannot connect, or neither app is installed on the TV.
pub(crate) fn open_app(host: &str) -> Result<()> {
    let adb = adb().ok_or_else(|| anyhow!("adb is not installed"))?;
    let serial = format!("{}:{ADB_PORT}", host.trim());
    let connect = run_adb(&adb, &["connect", &serial])?;
    if connect.contains("failed") || connect.contains("unable") {
        bail!("adb could not connect to the TV: {}", connect.trim());
    }
    run_adb(&adb, &["-s", &serial, "shell", "input", "keyevent", "KEYCODE_WAKEUP"])?;
    for package in PACKAGES {
        let resolved =
            run_adb(&adb, &["-s", &serial, "shell", "cmd", "package", "resolve-activity", "--brief", package])?;
        if let Some(activity) = launch_activity(&resolved, package) {
            // The other app (the old one, while both are installed) keeps its listener on port 7878 in the
            // background, so the one opened here could not take it: stop it first. A missing package is a no-op.
            for other in PACKAGES.iter().filter(|p| **p != package) {
                let _ = run_adb(&adb, &["-s", &serial, "shell", "am", "force-stop", other]);
            }
            run_adb(&adb, &["-s", &serial, "shell", "am", "start", "-n", activity])?;
            return Ok(());
        }
    }
    bail!("the TunnelRez TV app is not installed on the TV")
}

/// The `package/activity` line of `cmd package resolve-activity --brief`, when the package is installed.
fn launch_activity<'a>(resolved: &'a str, package: &str) -> Option<&'a str> {
    resolved.lines().map(str::trim).rfind(|l| l.starts_with(package) && l.contains('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_activity_of_an_installed_package() {
        let installed = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\n\
                         dev.tunnelrez.tv/android.app.NativeActivity\n";
        assert_eq!(launch_activity(installed, PACKAGES[0]), Some("dev.tunnelrez.tv/android.app.NativeActivity"));
        assert_eq!(launch_activity(installed, PACKAGES[1]), None, "another package's line does not count");
        assert_eq!(launch_activity("No activity found\n", PACKAGES[0]), None);
    }
}
