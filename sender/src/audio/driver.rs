//! The Steam Streaming Speakers driver: `TunnelRez`'s virtual sink ("TV speaker"). The repo and the kit carry an
//! unchanged copy (`drivers/steam-streaming-speakers/x64/`); a Steam install has the same files under
//! `%CommonProgramFiles(x86)%\Steam\drivers\Windows10\x64\`.
//!
//! Install the way Sunshine and Apollo do it: `DiInstallDriverW` on the INF. Its `[DeviceInstall32]` section
//! creates the root device `ROOT\SteamStreamingSpeakers\0000` and installs the driver on it. Should no device
//! appear (an older Windows without that section's support), the device is created with `SetupDi` and the driver
//! installed on it with `UpdateDriverForPlugAndPlayDevicesW`, as `devcon install` does. Both need an elevated
//! process: `tunnelrez-send audio-sink install` asks once (UAC) and runs itself elevated for that step.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// The INF's hardware ID for the root-enumerated speakers device.
pub const HARDWARE_ID: &str = r"ROOT\SteamStreamingSpeakers";
/// The speakers INF, in either folder.
pub const INF_NAME: &str = "SteamStreamingSpeakers.inf";
/// The endpoint name the device gets (what the sink finder matches).
pub const ENDPOINT_NAME: &str = "Steam Streaming Speakers";
/// The bundled folder, relative to the exe (the kit) or to the repo root (a source checkout).
const BUNDLED: [&str; 3] = ["drivers", "steam-streaming-speakers", "x64"];
/// How many folders above the exe to look for the repo's `drivers` folder (`sender\target\release` -> repo).
const ANCESTORS: usize = 4;

/// Where an INF was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The copy shipped with `TunnelRez` (beside the exe, or in the source checkout).
    Bundled,
    /// A Steam install's copy.
    Steam,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Bundled => "bundled",
            Source::Steam => "steam",
        }
    }
}

/// The speakers INF to install from: the bundled copy beside `exe_dir` or in a folder above it, else the Steam
/// copy under `common_x86` (`%CommonProgramFiles(x86)%`).
pub fn find_inf(
    exe_dir: &Path,
    common_x86: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, Source)> {
    let bundled = exe_dir
        .ancestors()
        .take(ANCESTORS + 1)
        .map(|d| BUNDLED.iter().fold(d.to_path_buf(), |p, c| p.join(c)).join(INF_NAME))
        .find(|p| exists(p))
        .map(|p| (p, Source::Bundled));
    bundled.or_else(|| {
        let p = common_x86?.join(r"Steam\drivers\Windows10\x64").join(INF_NAME);
        exists(&p).then_some((p, Source::Steam))
    })
}

/// [`find_inf`] for this process.
pub fn locate() -> Option<(PathBuf, Source)> {
    let exe = std::env::current_exe().ok()?;
    let common = std::env::var_os("CommonProgramFiles(x86)").map(PathBuf::from);
    find_inf(exe.parent()?, common.as_deref(), Path::is_file)
}

/// The device-installation calls (mocked in tests; the real ones need an elevated process).
pub trait Setup {
    /// A `ROOT\SteamStreamingSpeakers` device is present.
    ///
    /// # Errors
    ///
    /// The device list cannot be read.
    fn device_present(&mut self) -> Result<bool>;
    /// `DiInstallDriverW(inf)`. Returns whether Windows wants a reboot.
    ///
    /// # Errors
    ///
    /// Windows refused (not elevated, bad signature, ...).
    fn install_inf(&mut self, inf: &Path) -> Result<bool>;
    /// Create the root device and install the INF on it. Returns whether Windows wants a reboot.
    ///
    /// # Errors
    ///
    /// Windows refused.
    fn create_device(&mut self, inf: &Path) -> Result<bool>;
    /// Remove every `ROOT\SteamStreamingSpeakers` device. Returns how many.
    ///
    /// # Errors
    ///
    /// Windows refused.
    fn remove_devices(&mut self) -> Result<usize>;
}

/// What [`install`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// The device was there already: nothing done.
    Already,
    /// Installed now; `via` names the path that worked.
    Now { reboot: bool, via: &'static str },
}

/// Install the speakers from `inf` unless the device is there already (idempotent).
///
/// # Errors
///
/// Windows refused, or no device appeared.
pub fn install(s: &mut dyn Setup, inf: &Path) -> Result<Installed> {
    if s.device_present()? {
        return Ok(Installed::Already);
    }
    let mut reboot = s.install_inf(inf).context("DiInstallDriverW")?;
    if s.device_present()? {
        return Ok(Installed::Now { reboot, via: "DiInstallDriver" });
    }
    reboot |= s.create_device(inf).context("create the root device")?;
    if s.device_present()? {
        return Ok(Installed::Now { reboot, via: "root device + UpdateDriverForPlugAndPlayDevices" });
    }
    bail!("the driver installed but no {HARDWARE_ID} device appeared")
}

/// The marker's file name.
const MARKER: &str = "steam-speakers.json";

/// `%LOCALAPPDATA%\TunnelRez\steam-speakers.json`: written when `TunnelRez` installed the device, so
/// `uninstall` only removes a device it added (Steam may own one too).
fn marker_path() -> Option<PathBuf> {
    super::tv_only::state_path(MARKER)
}

/// The marker an install from before the rename left (`%LOCALAPPDATA%\SlipStream`): it counts as ours too.
fn legacy_marker_path() -> Option<PathBuf> {
    Some(crate::paths::legacy_local_dir()?.join(MARKER))
}

fn installed_by_us() -> bool {
    [marker_path(), legacy_marker_path()].into_iter().flatten().any(|p| p.is_file())
}

/// Forget that we installed the device (both markers).
fn remove_markers() {
    for p in [marker_path(), legacy_marker_path()].into_iter().flatten() {
        let _ = std::fs::remove_file(p);
    }
}

/// The speakers' playback endpoints (ID and name) Windows has now.
fn speaker_endpoints() -> Vec<super::sink::DeviceInfo> {
    let Ok(_com) = super::wasapi::Com::init() else { return Vec::new() };
    super::wasapi::render_devices()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.name.contains(ENDPOINT_NAME) || d.adapter.contains(ENDPOINT_NAME))
        .collect()
}

/// The friendly name of the speakers' playback endpoint, when Windows has one.
fn endpoint_name() -> Option<String> {
    speaker_endpoints().into_iter().next().map(|d| d.name)
}

/// The install left Windows wanting a restart (from the marker), and the endpoint is not there yet.
fn reboot_pending(endpoint: Option<&str>) -> bool {
    endpoint.is_none()
        && marker_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|v| v.get("reboot") == Some(&Value::Bool(true)))
}

/// `audio-sink status`: one JSON line.
///
/// # Errors
///
/// The device list cannot be read.
pub fn status_json() -> Result<String> {
    let device = win::Real.device_present()?;
    let inf = locate();
    let endpoint = endpoint_name();
    Ok(json!({
        "device": device,
        // Installed and usable: Windows has its playback endpoint.
        "ready": device && endpoint.is_some(),
        "reboot_required": device && reboot_pending(endpoint.as_deref()),
        "endpoint": endpoint,
        "inf": inf.as_ref().map(|(p, _)| p.display().to_string()),
        "source": inf.map(|(_, s)| s.name()),
        "installed_by_tunnelrez": installed_by_us(),
        "elevated": win::is_elevated(),
    })
    .to_string())
}

/// The steps `install` would take now (`audio-sink install --dry-run`); nothing is changed.
///
/// # Errors
///
/// The device list cannot be read.
pub fn plan() -> Result<String> {
    if win::Real.device_present()? {
        return Ok(format!("{HARDWARE_ID} is installed: nothing to do"));
    }
    let Some((inf, src)) = locate() else { bail!("no {INF_NAME} found (bundled or Steam)") };
    Ok([
        format!("would install from {} ({} copy)", inf.display(), src.name()),
        format!(
            "{}: DiInstallDriverW, then, if no {HARDWARE_ID} device appears, SetupDi root device + \
             UpdateDriverForPlugAndPlayDevicesW",
            if win::is_elevated() { "already elevated" } else { "one UAC prompt, then elevated" }
        ),
        "then: wait for the playback endpoint and put the default playback devices back".into(),
    ]
    .join("\n"))
}

/// The defaults of the three roles now (to put back after the install, as Sunshine does).
fn defaults_now() -> Vec<(super::sink::Role, String)> {
    let Ok(_com) = super::wasapi::Com::init() else { return Vec::new() };
    super::sink::Role::ALL.iter().filter_map(|r| Some((*r, super::wasapi::default_id(r.erole())?))).collect()
}

/// Whether the speakers' device exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Present,
    Absent,
}

/// What putting the defaults back after an install came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// Every role that Windows moved to the speakers is back (or none moved).
    Done,
    /// The speakers' endpoint is not there yet (Windows may still add it, and make it the default): keep the
    /// record for a later try.
    Waiting,
    /// Some roles could not be put back: keep the record.
    Failed(Vec<String>),
}

/// Put back each role that now points at one of `speakers` (the new device's endpoints) to its default from
/// before the install. A role the user (or nothing) changed to anything else stays, and so does a role on an
/// endpoint in `owned` (a running stream's virtual sink).
pub fn restore_install_defaults(
    before: &[(super::sink::Role, String)],
    speakers: &[String],
    owned: &[String],
    device: Device,
    mut current: impl FnMut(super::sink::Role) -> Option<String>,
    mut set: impl FnMut(&str, super::sink::Role) -> Result<()>,
) -> Settled {
    if speakers.is_empty() {
        return if device == Device::Present { Settled::Waiting } else { Settled::Done };
    }
    let (mut failed, mut skipped) = (Vec::new(), false);
    for (r, old) in before {
        let now = current(*r);
        if now.as_deref().is_some_and(|n| n != old && speakers.iter().any(|s| s == n)) {
            if owned.iter().any(|o| Some(o.as_str()) == now.as_deref()) {
                skipped = true; // a running stream switched it there: leave it, try again later
                continue;
            }
            if let Err(e) = set(old, *r) {
                failed.push(format!("{r:?}: {e:#}"));
            }
        }
    }
    if !failed.is_empty() {
        Settled::Failed(failed)
    } else if skipped {
        Settled::Waiting
    } else {
        Settled::Done
    }
}

/// `%LOCALAPPDATA%\TunnelRez\install-defaults.json`: the defaults from before an install, until they are
/// back. Written before the UAC prompt, so a killed install is finished by the next start or `audio-restore`.
fn defaults_path() -> Option<PathBuf> {
    super::tv_only::state_path("install-defaults.json")
}

fn save_defaults(before: &[(super::sink::Role, String)]) -> Result<()> {
    let path = defaults_path().context("LOCALAPPDATA is not set")?;
    let roles: serde_json::Map<String, Value> =
        before.iter().map(|(r, id)| (format!("{r:?}"), Value::String(id.clone()))).collect();
    super::tv_only::write_atomic(&path, &json!({ "before": roles, "pid": std::process::id() }).to_string())
        .with_context(|| format!("write {}", path.display()))
}

/// Add the elevated installer's pid to the saved defaults, so recovery waits while it still runs.
fn note_installer(pid: u32) {
    let Some(path) = defaults_path() else { return };
    let Some(mut v) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return;
    };
    v["installer"] = json!(pid);
    let _ = super::tv_only::write_atomic(&path, &v.to_string());
}

/// The install is still going: its elevated installer runs, or another sender that started it still runs (it
/// settles the defaults itself when the installer is done). `alive` must say yes for a process it cannot
/// inspect (an installer elevated with another administrator's credentials).
fn install_running(v: &Value, me: u32, alive: impl Fn(u32) -> bool) -> bool {
    let pid = |k: &str| v.get(k).and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok());
    pid("installer").is_some_and(&alive) || pid("pid").is_some_and(|p| p != me && alive(p))
}

fn load_defaults() -> Option<Vec<(super::sink::Role, String)>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(defaults_path()?).ok()?).ok()?;
    let b = v.get("before")?.as_object()?;
    Some(
        super::sink::Role::ALL
            .iter()
            .filter_map(|r| Some((*r, b.get(&format!("{r:?}"))?.as_str()?.to_string())))
            .collect(),
    )
}

/// Try the saved defaults once (Windows side); remove the record when done.
fn settle_saved() -> Option<Settled> {
    settle_saved_with(&super::sink::live_route_sink().into_iter().collect::<Vec<_>>())
}

/// [`settle_saved`] with the sink endpoints running streams own (the caller that holds the sink switch's lock
/// passes them, so no stream can take the sink while this runs).
fn settle_saved_with(owned: &[String]) -> Option<Settled> {
    let before = load_defaults()?;
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(defaults_path()?).ok()?).ok()?;
    let not_gone = |p| crate::winutil::process_state(p) != crate::winutil::ProcessState::Gone;
    if install_running(&raw, std::process::id(), not_gone) {
        return Some(Settled::Waiting);
    }
    let present = if win::Real.device_present().unwrap_or(true) { Device::Present } else { Device::Absent };
    let speakers: Vec<String> = speaker_endpoints().into_iter().map(|d| d.id).collect();
    let _com = super::wasapi::Com::init().ok()?;
    let policy = super::policy::Policy::new().ok();
    let r = restore_install_defaults(
        &before,
        &speakers,
        owned,
        present,
        |role| super::wasapi::default_id(role.erole()),
        |id, role| policy.as_ref().context("no default-device control")?.set_default(id, role.erole()),
    );
    if r == Settled::Done {
        if let Some(p) = defaults_path() {
            let _ = std::fs::remove_file(p);
        }
    }
    Some(r)
}

/// After an install: wait up to 10 s for the endpoint, then put the defaults back.
fn settle_defaults() -> Option<Settled> {
    let t0 = std::time::Instant::now();
    while endpoint_name().is_none() && t0.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    settle_saved()
}

/// At start and in `audio-restore`: finish putting the defaults back after an earlier install (killed, or the
/// endpoint came later). A log line when there was something to do.
pub fn recover_stale() -> Option<String> {
    recovery_line(settle_saved()?)
}

/// [`recover_stale`] for the sink module, called while it holds its switch's lock: `owned` are the sink endpoints
/// other processes' streams own right now.
pub(super) fn recover_stale_locked(owned: &[String]) -> Option<String> {
    recovery_line(settle_saved_with(owned)?)
}

fn recovery_line(s: Settled) -> Option<String> {
    match s {
        Settled::Done => Some("default playback devices put back after the TV speaker install".into()),
        Settled::Waiting => None,
        Settled::Failed(f) => Some(format!("could not yet put back after the TV speaker install: {}", f.join("; "))),
    }
}

/// `audio-sink install` (`--dry-run` is [`plan`]).
///
/// # Errors
///
/// No INF, the UAC prompt was declined, or Windows refused.
pub fn install_command() -> Result<String> {
    if win::Real.device_present()? {
        return Ok(format!("TV speaker already installed ({HARDWARE_ID})"));
    }
    let (inf, src) = locate().with_context(|| format!("no {INF_NAME} found (bundled or Steam)"))?;
    // Saved before anything changes, and kept until every role is back (see `recover_stale`).
    save_defaults(&defaults_now())?;
    let result = elevated("install", &inf);
    let settled = settle_defaults();
    let Done { msg, reboot, created } = result?;
    // Only a device this run created is ours to remove later; one another installer added meanwhile is not.
    if created {
        if let Some(p) = marker_path() {
            let marker = json!({ "inf": inf.display().to_string(), "reboot": reboot });
            let _ = super::tv_only::write_atomic(&p, &marker.to_string());
        }
    }
    let mut out = format!("TV speaker installed from the {} copy: {msg}", src.name());
    if reboot {
        out.push_str(". Restart Windows to finish (REBOOT_REQUIRED)");
    }
    match settled {
        Some(Settled::Failed(f)) => {
            let _ = write!(out, "; default devices not all back yet ({})", f.join("; "));
        }
        Some(Settled::Waiting) if !reboot => out.push_str("; the playback endpoint is not there yet"),
        _ => {}
    }
    Ok(out)
}

/// Which device `audio-sink uninstall` may remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remove {
    /// Only one `TunnelRez` installed (Steam may own the device too).
    Ours,
    /// Any (`--force`).
    Any,
}

/// `audio-sink uninstall [--force]`.
///
/// # Errors
///
/// Not installed by `TunnelRez` (with [`Remove::Ours`]), the UAC prompt was declined, or Windows refused.
pub fn uninstall_command(which: Remove) -> Result<String> {
    let force = which == Remove::Any;
    if !win::Real.device_present()? {
        remove_markers();
        return Ok("TV speaker not installed: nothing to do".into());
    }
    if !force && !installed_by_us() {
        bail!("the {HARDWARE_ID} device was not installed by TunnelRez (Steam may use it); --force removes it anyway");
    }
    let r = elevated("uninstall", Path::new(""))?.msg;
    remove_markers();
    Ok(format!("TV speaker removed: {r}"))
}

/// What the elevated step did.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Done {
    msg: String,
    /// Windows wants a restart.
    reboot: bool,
    /// The install created the device now (false: it was there already when the elevated step looked).
    created: bool,
}

/// Run `action` elevated: directly when this process is, else this exe again with one UAC prompt
/// (`audio-sink elevated <action> <inf> <result file>`). Returns the child's result.
fn elevated(action: &str, inf: &Path) -> Result<Done> {
    if win::is_elevated() {
        return run_elevated_action(action, inf);
    }
    let result = std::env::temp_dir().join(format!("tunnelrez-audio-sink-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&result);
    let exe = std::env::current_exe()?;
    let args = vec![
        "audio-sink".to_string(),
        "elevated".into(),
        action.into(),
        inf.display().to_string(),
        result.display().to_string(),
    ];
    let code = win::run_as_admin(&exe, &args, note_installer)?;
    let text = std::fs::read_to_string(&result).unwrap_or_default();
    let _ = std::fs::remove_file(&result);
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("no result").to_string();
    if code != 0 || v.get("ok") != Some(&Value::Bool(true)) {
        bail!("{msg} (exit code {code})");
    }
    let flag = |k: &str| v.get(k) == Some(&Value::Bool(true));
    Ok(Done { msg, reboot: flag("reboot"), created: flag("created") })
}

/// The elevated work.
fn run_elevated_action(action: &str, inf: &Path) -> Result<Done> {
    let mut s = win::Real;
    match action {
        "install" => Ok(done_of(install(&mut s, inf)?)),
        "uninstall" => {
            Ok(Done { msg: format!("{} device(s) removed", s.remove_devices()?), reboot: false, created: false })
        }
        x => bail!("unknown elevated action {x}"),
    }
}

fn done_of(i: Installed) -> Done {
    match i {
        Installed::Already => Done { msg: "already present".into(), reboot: false, created: false },
        Installed::Now { reboot, via } => Done { msg: format!("installed via {via}"), reboot, created: true },
    }
}

/// The elevated child (`audio-sink elevated <action> <inf> <result file>`): do the work and write
/// `{"ok":bool,"msg":...,"reboot":bool,"created":bool}` to the result file (an elevated process's output cannot be read by its parent).
pub fn elevated_child(action: &str, inf: &str, result: &str) -> i32 {
    let r = run_elevated_action(action, Path::new(inf));
    let (ok, d) = match r {
        Ok(d) => (true, d),
        Err(e) => (false, Done { msg: format!("{e:#}"), reboot: false, created: false }),
    };
    let v = json!({ "ok": ok, "msg": d.msg, "reboot": d.reboot, "created": d.created });
    let _ = std::fs::write(result, v.to_string());
    i32::from(!ok)
}

/// The real `SetupDi` / newdev calls, elevation check and the UAC relaunch.
pub(crate) mod win {
    use super::{Setup, HARDWARE_ID};
    use anyhow::{bail, Context, Result};
    use std::path::Path;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DiInstallDriverW, DiUninstallDevice, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList,
        SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        SetupDiGetDeviceRegistryPropertyW, SetupDiGetINFClassW, SetupDiSetDeviceRegistryPropertyW,
        UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIF_REMOVE, DIGCF_PRESENT,
        DIINSTALLDRIVER_FLAGS, GUID_DEVCLASS_MEDIA, HDEVINFO, INSTALLFLAG_FORCE, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
    };
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, GetProcessId, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NO_CONSOLE, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    /// A device-info set, destroyed on drop.
    struct Set(HDEVINFO);

    impl Drop for Set {
        fn drop(&mut self) {
            // SAFETY: we own the set; it is not used after this.
            let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }

    fn data() -> SP_DEVINFO_DATA {
        SP_DEVINFO_DATA {
            cbSize: u32::try_from(std::mem::size_of::<SP_DEVINFO_DATA>()).unwrap_or(0),
            ..Default::default()
        }
    }

    /// A `REG_MULTI_SZ` buffer (UTF-16 LE bytes) -> its strings.
    pub(crate) fn multi_sz(b: &[u8]) -> Vec<String> {
        let w: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        w.split(|c| *c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }

    /// `HARDWARE_ID` as a `REG_MULTI_SZ` buffer.
    pub(super) fn hardware_id_multi_sz() -> Vec<u8> {
        HARDWARE_ID.encode_utf16().chain([0, 0]).flat_map(u16::to_le_bytes).collect()
    }

    /// Present MEDIA-class devices whose hardware IDs include [`HARDWARE_ID`], as (set, members).
    fn speakers() -> Result<(Set, Vec<SP_DEVINFO_DATA>)> {
        // SAFETY: a valid class GUID; returns an owned set (destroyed by `Set`).
        let set = Set(unsafe { SetupDiGetClassDevsW(Some(&GUID_DEVCLASS_MEDIA), PCWSTR::null(), None, DIGCF_PRESENT) }
            .context("SetupDiGetClassDevsW")?);
        let mut found = Vec::new();
        for i in 0.. {
            let mut d = data();
            // SAFETY: valid set and out-struct with cbSize set; fails at the end of the list.
            if unsafe { SetupDiEnumDeviceInfo(set.0, i, &raw mut d) }.is_err() {
                break;
            }
            let mut buf = vec![0u8; 1024];
            let mut need = 0u32;
            // SAFETY: valid set and member; the buffer and size out-pointer are valid for the call.
            let got = unsafe {
                SetupDiGetDeviceRegistryPropertyW(
                    set.0,
                    &raw const d,
                    SPDRP_HARDWAREID,
                    None,
                    Some(&mut buf),
                    Some(&raw mut need),
                )
            };
            if got.is_ok() {
                let len = usize::try_from(need).unwrap_or(0).min(buf.len());
                if multi_sz(&buf[..len]).iter().any(|h| h.eq_ignore_ascii_case(HARDWARE_ID)) {
                    found.push(d);
                }
            }
        }
        Ok((set, found))
    }

    /// The real Windows calls.
    #[derive(Debug)]
    pub(super) struct Real;

    impl Setup for Real {
        fn device_present(&mut self) -> Result<bool> {
            Ok(!speakers()?.1.is_empty())
        }

        fn install_inf(&mut self, inf: &Path) -> Result<bool> {
            let mut reboot = windows::core::BOOL(0);
            // SAFETY: the path string outlives the call; the reboot flag is a valid out-pointer.
            unsafe {
                DiInstallDriverW(None, &HSTRING::from(inf.as_os_str()), DIINSTALLDRIVER_FLAGS(0), Some(&raw mut reboot))
            }?;
            Ok(reboot.as_bool())
        }

        fn create_device(&mut self, inf: &Path) -> Result<bool> {
            let inf_w = HSTRING::from(inf.as_os_str());
            let mut guid = windows::core::GUID::zeroed();
            let mut class = [0u16; 64];
            // SAFETY: valid INF path, GUID and name buffer.
            unsafe { SetupDiGetINFClassW(&inf_w, &raw mut guid, &mut class, None) }.context("SetupDiGetINFClassW")?;
            // SAFETY: valid class GUID; returns an owned set (destroyed by `Set`).
            let set = Set(unsafe { SetupDiCreateDeviceInfoList(Some(&raw const guid), None) }?);
            let mut d = data();
            // SAFETY: valid set; the class name is NUL-terminated (from SetupDiGetINFClassW); out-struct valid.
            unsafe {
                SetupDiCreateDeviceInfoW(
                    set.0,
                    PCWSTR(class.as_ptr()),
                    &raw const guid,
                    PCWSTR::null(),
                    None,
                    DICD_GENERATE_ID,
                    Some(&raw mut d),
                )
            }
            .context("SetupDiCreateDeviceInfoW")?;
            let ids = hardware_id_multi_sz();
            // SAFETY: valid set and member; the buffer is a REG_MULTI_SZ.
            unsafe { SetupDiSetDeviceRegistryPropertyW(set.0, &raw mut d, SPDRP_HARDWAREID, Some(&ids)) }
                .context("set the hardware ID")?;
            // SAFETY: valid set and member.
            unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&raw const d)) }
                .context("DIF_REGISTERDEVICE")?;
            let mut reboot = windows::core::BOOL(0);
            // SAFETY: valid strings and out-pointer.
            let updated = unsafe {
                UpdateDriverForPlugAndPlayDevicesW(
                    None,
                    &HSTRING::from(HARDWARE_ID),
                    &inf_w,
                    INSTALLFLAG_FORCE,
                    Some(&raw mut reboot),
                )
            };
            if let Err(e) = updated {
                // SAFETY: removes the device registered above.
                let _ = unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, Some(&raw const d)) };
                return Err(e).context("UpdateDriverForPlugAndPlayDevicesW");
            }
            Ok(reboot.as_bool())
        }

        fn remove_devices(&mut self) -> Result<usize> {
            let (set, found) = speakers()?;
            for d in &found {
                // SAFETY: valid set and a member of it.
                unsafe { DiUninstallDevice(HWND::default(), set.0, d, 0, None) }.context("DiUninstallDevice")?;
            }
            Ok(found.len())
        }
    }

    /// This process runs elevated (admin token).
    pub(crate) fn is_elevated() -> bool {
        let mut token = HANDLE::default();
        // SAFETY: the current-process pseudo handle; the token handle is closed below.
        // SAFETY: returns the current-process pseudo handle (no cleanup needed).
        let me = unsafe { GetCurrentProcess() };
        // SAFETY: a valid process handle and out-pointer.
        if unsafe { OpenProcessToken(me, TOKEN_QUERY, &raw mut token) }.is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let size = u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>()).unwrap_or(0);
        // SAFETY: valid token; the buffer is a TOKEN_ELEVATION of `size` bytes.
        let ok = unsafe { GetTokenInformation(token, TokenElevation, Some((&raw mut e).cast()), size, &raw mut len) };
        // SAFETY: the token handle is ours and not used after this.
        let _ = unsafe { CloseHandle(token) };
        ok.is_ok() && e.TokenIsElevated != 0
    }

    /// Quote one command-line argument the way `CommandLineToArgvW` reads it.
    pub(super) fn quote(a: &str) -> String {
        if !a.is_empty() && !a.contains([' ', '\t', '"']) {
            return a.to_string();
        }
        let mut s = String::from('"');
        let mut slashes = 0;
        for c in a.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    s.push_str(&"\\".repeat(slashes * 2 + 1));
                    s.push('"');
                    slashes = 0;
                }
                _ => {
                    s.push_str(&"\\".repeat(slashes));
                    s.push(c);
                    slashes = 0;
                }
            }
        }
        s.push_str(&"\\".repeat(slashes * 2));
        s.push('"');
        s
    }

    /// Run `exe args` elevated (one UAC prompt, no window) and wait. Returns its exit code.
    pub(crate) fn run_as_admin(exe: &Path, args: &[String], started: impl FnOnce(u32)) -> Result<u32> {
        let file = HSTRING::from(exe.as_os_str());
        let params = HSTRING::from(args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
        let verb = HSTRING::from("runas");
        let mut info = SHELLEXECUTEINFOW {
            cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE | SEE_MASK_FLAG_NO_UI,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        // SAFETY: `info` is fully initialized and its strings outlive the call.
        if let Err(e) = unsafe { ShellExecuteExW(&raw mut info) } {
            bail!("the administrator prompt was declined or failed: {e}");
        }
        if info.hProcess.is_invalid() {
            bail!("no elevated process");
        }
        // SAFETY: a valid process handle (SEE_MASK_NOCLOSEPROCESS).
        started(unsafe { GetProcessId(info.hProcess) });
        // SAFETY: the process handle is ours (SEE_MASK_NOCLOSEPROCESS); closed below.
        unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
        let mut code = 1u32;
        // SAFETY: valid handle; valid out-pointer.
        let _ = unsafe { GetExitCodeProcess(info.hProcess, &raw mut code) };
        // SAFETY: ours, not used after this.
        let _ = unsafe { CloseHandle(info.hProcess) };
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_bundled_copy_wins_beside_the_exe_or_in_the_checkout() {
        let kit = Path::new(r"C:\kit");
        let have: HashSet<PathBuf> =
            [kit.join(r"drivers\steam-streaming-speakers\x64").join(INF_NAME)].into_iter().collect();
        let common = Path::new(r"C:\CF");
        let r = find_inf(kit, Some(common), |p| have.contains(p));
        assert_eq!(r, Some((kit.join(r"drivers\steam-streaming-speakers\x64").join(INF_NAME), Source::Bundled)));
        // A source checkout: the exe is in sender\target\release, the folder at the repo root.
        let repo = Path::new(r"C:\repo");
        let have: HashSet<PathBuf> =
            [repo.join(r"drivers\steam-streaming-speakers\x64").join(INF_NAME)].into_iter().collect();
        let r = find_inf(&repo.join(r"sender\target\release"), Some(common), |p| have.contains(p));
        assert_eq!(r.map(|x| x.1), Some(Source::Bundled));
    }

    #[test]
    fn the_steam_copy_is_the_fallback() {
        let common = Path::new(r"C:\CF");
        let steam = common.join(r"Steam\drivers\Windows10\x64").join(INF_NAME);
        let r = find_inf(Path::new(r"C:\kit"), Some(common), |p| p == steam.as_path());
        assert_eq!(r, Some((steam, Source::Steam)));
        assert_eq!(find_inf(Path::new(r"C:\kit"), Some(common), |_| false), None);
        assert_eq!(find_inf(Path::new(r"C:\kit"), None, |_| false), None);
    }

    /// A fake device manager: what each call does to "the device is present".
    #[derive(Default)]
    struct Fake {
        present: bool,
        /// `DiInstallDriver` creates the device (Windows 10 1903+, `[DeviceInstall32]`).
        inf_creates: bool,
        /// The `SetupDi` fallback works.
        create_works: bool,
        calls: Vec<&'static str>,
    }

    impl Setup for Fake {
        fn device_present(&mut self) -> Result<bool> {
            Ok(self.present)
        }
        fn install_inf(&mut self, _: &Path) -> Result<bool> {
            self.calls.push("DiInstallDriver");
            self.present |= self.inf_creates;
            Ok(false)
        }
        fn create_device(&mut self, _: &Path) -> Result<bool> {
            self.calls.push("create_device");
            if self.create_works {
                self.present = true;
                Ok(true)
            } else {
                bail!("refused")
            }
        }
        fn remove_devices(&mut self) -> Result<usize> {
            self.calls.push("remove");
            Ok(usize::from(std::mem::take(&mut self.present)))
        }
    }

    #[test]
    fn install_is_idempotent() {
        let mut f = Fake { present: true, ..Fake::default() };
        assert_eq!(install(&mut f, Path::new("x.inf")).unwrap(), Installed::Already);
        assert!(f.calls.is_empty(), "nothing done: {:?}", f.calls);
    }

    #[test]
    fn install_uses_diinstalldriver_first() {
        let mut f = Fake { inf_creates: true, ..Fake::default() };
        assert_eq!(
            install(&mut f, Path::new("x.inf")).unwrap(),
            Installed::Now { reboot: false, via: "DiInstallDriver" }
        );
        assert_eq!(f.calls, ["DiInstallDriver"]);
        assert_eq!(install(&mut f, Path::new("x.inf")).unwrap(), Installed::Already, "second run does nothing");
    }

    #[test]
    fn install_falls_back_to_a_root_device() {
        let mut f = Fake { create_works: true, ..Fake::default() };
        let r = install(&mut f, Path::new("x.inf")).unwrap();
        assert!(matches!(r, Installed::Now { reboot: true, .. }), "{r:?}");
        assert_eq!(f.calls, ["DiInstallDriver", "create_device"]);
        let mut f = Fake::default();
        assert!(install(&mut f, Path::new("x.inf")).is_err(), "no device at all: an error, not a silent success");
    }

    #[test]
    fn install_defaults_come_back_only_from_the_speakers() {
        use crate::audio::sink::Role;
        let before = vec![
            (Role::Console, "{spk}".to_string()),
            (Role::Multimedia, "{spk}".to_string()),
            (Role::Communications, "{headset}".to_string()),
        ];
        let speakers = vec!["{steam}".to_string()];
        // Windows made the speakers the default for console and communications; multimedia the user changed.
        let now = |r: Role| {
            Some(match r {
                Role::Console | Role::Communications => "{steam}".to_string(),
                Role::Multimedia => "{hdmi}".to_string(),
            })
        };
        let mut set = Vec::new();
        let r = restore_install_defaults(&before, &speakers, &[], Device::Present, now, |id, r| {
            set.push((r, id.to_string()));
            Ok(())
        });
        assert_eq!(r, Settled::Done);
        assert_eq!(set, [(Role::Console, "{spk}".to_string()), (Role::Communications, "{headset}".to_string())]);
        // A refused switch keeps the record for later.
        let r = restore_install_defaults(&before, &speakers, &[], Device::Present, now, |_, _| bail!("refused"));
        assert!(matches!(r, Settled::Failed(ref f) if f.len() == 2), "{r:?}");
        // No endpoint yet: wait (Windows may still add it); no device at all: nothing to undo.
        assert_eq!(restore_install_defaults(&before, &[], &[], Device::Present, now, |_, _| Ok(())), Settled::Waiting);
        assert_eq!(restore_install_defaults(&before, &[], &[], Device::Absent, now, |_, _| Ok(())), Settled::Done);
        // A running stream switched the roles to the speakers as its sink: they stay, and the record stays.
        let mut touched = false;
        let r = restore_install_defaults(&before, &speakers, &speakers, Device::Present, now, |_, _| {
            touched = true;
            Ok(())
        });
        assert_eq!((r, touched), (Settled::Waiting, false));
    }

    #[test]
    fn recovery_waits_while_the_install_runs() {
        let v = json!({"before": {}, "pid": 10, "installer": 11});
        assert!(install_running(&v, 99, |p| p == 11), "the elevated installer still runs");
        assert!(install_running(&v, 99, |p| p == 10), "the sender that started it still runs");
        assert!(!install_running(&v, 10, |p| p == 10), "that sender is us: settle");
        assert!(!install_running(&v, 99, |_| false), "both gone: recover");
        assert!(!install_running(&json!({"before": {}}), 99, |_| true), "an old record without pids");
    }

    #[test]
    fn only_a_device_created_now_is_ours() {
        assert!(!done_of(Installed::Already).created, "another installer added it during the prompt");
        let d = done_of(Installed::Now { reboot: true, via: "DiInstallDriver" });
        assert!(d.created && d.reboot);
    }

    #[test]
    fn hardware_id_buffers_round_trip() {
        let b = win::hardware_id_multi_sz();
        assert_eq!(win::multi_sz(&b), [HARDWARE_ID]);
        let two: Vec<u8> = "A\0B\0\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(win::multi_sz(&two), ["A", "B"]);
    }

    #[test]
    fn elevated_arguments_are_quoted_for_argv() {
        assert_eq!(win::quote("install"), "install");
        assert_eq!(win::quote(r"C:\Program Files\x\a.inf"), r#""C:\Program Files\x\a.inf""#);
        assert_eq!(win::quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(win::quote(""), r#""""#);
    }

    /// The bundled files are the unchanged originals (`drivers/steam-streaming-speakers/SHA256SUMS`).
    #[test]
    fn bundled_driver_files_match_their_hashes() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\drivers\steam-streaming-speakers");
        let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).expect("SHA256SUMS");
        let mut n = 0;
        for line in sums.lines().filter(|l| !l.trim().is_empty()) {
            let (want, file) = line.split_once("  ").expect("sha256sum format");
            let bytes = std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(crate::audio::sha256::hex(&bytes), want, "{file} changed");
            n += 1;
        }
        assert_eq!(n, 6, "speakers and microphone: inf, sys, cat each");
    }
}
