//! The Parsec Virtual Display Driver: `TunnelRez`'s virtual monitor ("TV monitor"). `vdd status` reads its health,
//! `vdd repair` fixes only what is wrong, `vdd install` runs the bundled, signed installer
//! (`drivers/parsec-vdd/`). Work that needs an administrator runs in this exe again, elevated, with one UAC prompt.
//!
//! Every Windows call is behind [`Fixer`], so the decisions (what is wrong, what to do, in which order) are tested
//! without touching the PC.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::audio::driver::win as elevation;

/// The adapter's hardware ID.
pub const HARDWARE_ID: &str = r"Root\Parsec\VDA";
// PINNED: see docs/PINNED.md (Parsec VDD 0.45.0.0 and its SHA-256)
/// The version the kit bundles.
pub const VERSION: &str = "0.45.0.0";
/// The bundled installer, relative to the exe (the kit) or to the repo root (a source checkout).
const BUNDLED: [&str; 3] = ["drivers", "parsec-vdd", "parsec-vdd-0.45.0.0.exe"];
/// SHA-256 of that installer (also in `drivers/parsec-vdd/SHA256SUMS`). It runs elevated, so it must be this file.
pub const INSTALLER_SHA256: &str = "e23332448fdaf5aa017cb308db5ef6855fac526a7ded05d80c039404126d5362";
/// How many folders above the exe to look for the repo's `drivers` folder (`sender\target\release` -> repo).
const ANCESTORS: usize = 4;
/// PCI vendor ID of NVIDIA, as `PreferredRenderAdapterVendorId` wants it.
pub const VENDOR_NVIDIA: u32 = 0x10DE;
/// How long the silent installer may run.
const INSTALLER_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(3);
/// Windows must restart before the device works (`CM_PROB_NEED_RESTART`).
const PROBLEM_NEED_RESTART: u32 = 14;

/// The adapter device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// No device: the driver is not installed, or its install broke.
    Missing,
    Disabled,
    /// Present, with this Device Manager problem code.
    Failed {
        code: u32,
    },
    Running,
}

impl Adapter {
    fn name(self) -> &'static str {
        match self {
            Adapter::Missing => "missing",
            Adapter::Disabled => "disabled",
            Adapter::Failed { .. } => "problem",
            Adapter::Running => "ok",
        }
    }

    fn problem(self) -> u32 {
        match self {
            Adapter::Failed { code } => code,
            _ => 0,
        }
    }
}

/// The registry values that make the driver render on the NVIDIA GPU.
///
/// What driver 0.45 does with them (disassembled `mm.dll`): it reads them once, when the device initialises, and
/// once the adapter is up it asks Windows for one render GPU: none when `DisablePreferredRenderAdapterChange` is not
/// 0; else `PreferredRenderAdapterLuid` when set (it wins, and a LUID changes at every boot, so it must not be used);
/// else the first GPU of `PreferredRenderAdapterVendorId`; else NVIDIA, AMD, then others. So a new value does nothing
/// until the adapter starts again, and plugging or unplugging a monitor does not start it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Render {
    /// An NVIDIA GPU is in this PC. Without one the values mean nothing and are left alone.
    pub nvidia_present: bool,
    /// `DisablePreferredRenderAdapterChange`.
    pub disable_change: Option<u32>,
    /// `PreferredRenderAdapterVendorId`.
    pub vendor_id: Option<u32>,
}

impl Render {
    /// The values say what they must, or do not matter here.
    #[must_use]
    pub fn ok(self) -> bool {
        !self.nvidia_present || (self.disable_change == Some(0) && self.vendor_id == Some(VENDOR_NVIDIA))
    }
}

/// What is known about the driver right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// The installed version, from its Apps entry. `None`: no Apps entry.
    pub version: Option<String>,
    pub adapter: Adapter,
    pub render: Render,
    /// Virtual monitors on the desktop.
    pub attached: Vec<String>,
    /// A plugged virtual monitor is not on the desktop.
    pub detached: bool,
    /// Monitors from earlier plugs that Windows remembers. Normal while nothing is plugged.
    pub phantom: usize,
    /// Plugged virtual monitors with a Device Manager problem.
    pub failed_monitors: usize,
}

/// The first thing wrong, most basic first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Nothing wrong.
    None,
    /// The driver is not installed.
    Missing,
    /// Windows must restart to finish.
    RestartWindows,
    /// The adapter is off in Device Manager.
    Disabled,
    /// The adapter or a monitor has a Device Manager problem.
    AdapterProblem,
    /// The monitor would render on the wrong GPU.
    Render,
    /// A plugged monitor is stuck off the desktop.
    Stuck,
}

impl Cause {
    fn name(self) -> &'static str {
        match self {
            Cause::None => "none",
            Cause::Missing => "missing",
            Cause::RestartWindows => "restart_windows",
            Cause::Disabled => "disabled",
            Cause::AdapterProblem => "adapter_problem",
            Cause::Render => "render",
            Cause::Stuck => "stuck",
        }
    }

    /// What the user can do about it.
    fn next(self) -> &'static str {
        match self {
            Cause::None => "none",
            Cause::Missing => "install",
            Cause::RestartWindows => "restart",
            Cause::Disabled | Cause::AdapterProblem | Cause::Render | Cause::Stuck => "repair",
        }
    }
}

impl Health {
    /// What to tell the user first.
    #[must_use]
    pub fn cause(&self) -> Cause {
        match self.adapter {
            Adapter::Missing => Cause::Missing,
            Adapter::Disabled => Cause::Disabled,
            Adapter::Failed { code } if code == PROBLEM_NEED_RESTART => Cause::RestartWindows,
            Adapter::Failed { .. } => Cause::AdapterProblem,
            Adapter::Running if self.failed_monitors > 0 => Cause::AdapterProblem,
            Adapter::Running if !self.render.ok() => Cause::Render,
            Adapter::Running if self.detached && self.attached.is_empty() => Cause::Stuck,
            Adapter::Running => Cause::None,
        }
    }

    /// One line for `vdd status` and the logs.
    fn to_json(&self, installer: Option<&Path>) -> Value {
        let cause = self.cause();
        json!({
            "driver_installed": self.version.is_some() || self.adapter != Adapter::Missing,
            "driver_version": self.version,
            "adapter": { "state": self.adapter.name(), "problem": self.adapter.problem() },
            "render_on_nvidia": {
                "applies": self.render.nvidia_present,
                "ok": self.render.ok(),
                "disable_change": self.render.disable_change,
                "vendor_id": self.render.vendor_id,
            },
            "monitors": {
                "attached": self.attached,
                "detached": self.detached,
                "phantom": self.phantom,
                "failed": self.failed_monitors,
            },
            "ready": cause == Cause::None,
            "cause": cause.name(),
            "next": cause.next(),
            "installer": installer.map(|p| p.display().to_string()),
        })
    }
}

/// One repair step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Enable the adapter in Device Manager.
    EnableAdapter,
    /// Restart the adapter (it takes every plugged monitor with it).
    RestartAdapter,
    /// Write the two render values.
    SetRender,
    /// Unplug every virtual monitor.
    ClearPlug,
}

impl Action {
    fn words(self) -> &'static str {
        match self {
            Action::EnableAdapter => "enable the adapter",
            Action::RestartAdapter => "restart the adapter",
            Action::SetRender => "set the render-on-NVIDIA values",
            Action::ClearPlug => "clear the stuck plug",
        }
    }

    /// Only the plug is ours to change without an administrator.
    fn needs_admin(self) -> bool {
        self != Action::ClearPlug
    }
}

/// Whether Windows must restart for a step to take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reboot {
    No,
    Needed,
}

/// Whether `repair` restarts a healthy adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Only when the adapter or a monitor shows a problem.
    IfNeeded,
    /// Also when nothing looks wrong: the screen still would not attach (`--restart-adapter`).
    Always,
}

/// The steps that fix `h`, in order. Empty: nothing to do.
///
/// # Errors
///
/// The driver is not installed: repair cannot help.
pub fn plan(h: &Health, restart: Restart) -> Result<Vec<Action>> {
    if h.adapter == Adapter::Missing {
        bail!("the Parsec driver is not installed: use vdd install");
    }
    let mut steps = Vec::new();
    if h.adapter == Adapter::Disabled {
        steps.push(Action::EnableAdapter);
    }
    let set_render = h.render.nvidia_present && !h.render.ok();
    if set_render {
        steps.push(Action::SetRender);
    }
    let broken = matches!(h.adapter, Adapter::Failed { code } if code != PROBLEM_NEED_RESTART) || h.failed_monitors > 0;
    // The driver reads the values only when the adapter starts (see `Render`).
    let waits_for_windows = h.adapter == Adapter::Failed { code: PROBLEM_NEED_RESTART };
    if broken || restart == Restart::Always || (set_render && !waits_for_windows) {
        steps.push(Action::RestartAdapter);
    } else if h.detached && h.attached.is_empty() {
        // A restart unplugs too; on its own, the plug needs no administrator.
        steps.push(Action::ClearPlug);
    }
    Ok(steps)
}

/// The driver and the PC (mocked in tests; most of the real ones need an elevated process).
pub trait Fixer {
    /// Read everything [`Health`] holds.
    ///
    /// # Errors
    ///
    /// The device list cannot be read.
    fn health(&mut self) -> Result<Health>;
    /// Do one repair step. Windows can accept it and still want a restart, which the next health read need not show.
    ///
    /// # Errors
    ///
    /// Windows refused.
    fn act(&mut self, action: Action) -> Result<Reboot>;
    /// Run the silent installer and wait; returns its exit code.
    ///
    /// # Errors
    ///
    /// It did not start, or did not end in time.
    fn run_installer(&mut self, exe: &Path) -> Result<u32>;
}

/// How a repair or install ended.
fn verdict(h: &Health, did: &str, reboot: Reboot) -> Result<String> {
    match (h.cause(), reboot) {
        (Cause::RestartWindows, _) | (_, Reboot::Needed) => {
            Ok(format!("{did}. Restart Windows to finish (REBOOT_REQUIRED)"))
        }
        (Cause::None, Reboot::No) => Ok(format!("TV monitor ok: {did}")),
        (c, Reboot::No) => bail!("{did}, but it still reports {}", c.name()),
    }
}

/// Which GPU a monitor on the desktop renders on, after the adapter restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    Nvidia,
    /// Another GPU, by name.
    Other(String),
    /// It could not be read, and why.
    Unknown(String),
}

/// What a failed render check says (the app offers the same repair again for it, not a reinstall).
pub const RENDER_STUCK: &str = "the TV monitor still renders on";

/// How a repair that restarted the adapter ended: only a monitor on the desktop shows if the GPU really changed.
fn confirm_render(done: String, seen: &Rendered) -> Result<String> {
    if done.contains("REBOOT_REQUIRED") {
        return Ok(done);
    }
    match seen {
        Rendered::Nvidia => Ok(format!("{done}. The TV monitor renders on NVIDIA")),
        Rendered::Other(gpu) => bail!(
            "{done}, but {RENDER_STUCK} {gpu}. Restart Windows, then press Repair again. The TV still works until \
             then: every frame is copied between the GPUs, so it is slower"
        ),
        Rendered::Unknown(why) => Ok(format!("{done}. The render GPU could not be checked: {why}")),
    }
}

/// Fix what is wrong (idempotent: a healthy driver is left alone).
///
/// # Errors
///
/// The driver is not installed, a step failed, or the problem is still there afterwards.
pub fn repair(fx: &mut dyn Fixer, restart: Restart) -> Result<String> {
    let steps = plan(&fx.health()?, restart)?;
    if steps.is_empty() {
        return Ok("TV monitor ok: nothing to repair".into());
    }
    let mut reboot = Reboot::No;
    for s in &steps {
        if fx.act(*s).with_context(|| format!("could not {}", s.words()))? == Reboot::Needed {
            reboot = Reboot::Needed;
        }
    }
    let did = steps.iter().map(|s| s.words()).collect::<Vec<_>>().join(", ");
    verdict(&fx.health()?, &did, reboot)
}

/// Whether `install` runs the installer over a working driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reinstall {
    /// Only when no driver is there.
    IfMissing,
    /// Always (`--force`): the driver looks fine and still does not work.
    Force,
}

/// Run the installer `exe` unless the driver is there, then set the render values (one UAC prompt covers both).
///
/// # Errors
///
/// The installer failed, or no adapter came.
pub fn install(fx: &mut dyn Fixer, exe: &Path, how: Reinstall) -> Result<String> {
    let h = fx.health()?;
    if how == Reinstall::IfMissing && h.adapter != Adapter::Missing {
        return repair(fx, Restart::IfNeeded);
    }
    check_installer(exe)?;
    let code = fx.run_installer(exe)?;
    if code != 0 {
        bail!("the Parsec installer failed (exit code {code})");
    }
    let h = fx.health()?;
    if h.adapter == Adapter::Missing {
        bail!("the Parsec installer finished, but no {HARDWARE_ID} adapter came");
    }
    let mut reboot = Reboot::No;
    if h.render.nvidia_present && !h.render.ok() {
        reboot = fx.act(Action::SetRender).context("could not set the render-on-NVIDIA values")?;
        // The installer started the adapter before the values were there (see `Render`).
        if h.adapter == Adapter::Running
            && fx.act(Action::RestartAdapter).context("could not restart the adapter")? == Reboot::Needed
        {
            reboot = Reboot::Needed;
        }
    }
    verdict(&fx.health()?, &format!("Parsec VDD {VERSION} installed"), reboot)
}

/// The bundled installer: beside `exe_dir` or in a folder above it.
pub fn find_installer(exe_dir: &Path, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    exe_dir
        .ancestors()
        .take(ANCESTORS + 1)
        .map(|d| BUNDLED.iter().fold(d.to_path_buf(), |p, c| p.join(c)))
        .find(|p| exists(p))
}

/// [`find_installer`] for this process.
#[must_use]
pub fn locate() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    find_installer(exe.parent()?, Path::is_file)
}

/// `exe` is the installer this build knows. It runs elevated, so nothing else may.
fn check_installer(exe: &Path) -> Result<()> {
    let bytes = std::fs::read(exe).with_context(|| format!("read {}", exe.display()))?;
    let got = crate::audio::sha256::hex(&bytes);
    if got != INSTALLER_SHA256 {
        bail!("{} is not the Parsec installer this build knows (SHA-256 {got})", exe.display());
    }
    Ok(())
}

/// `vdd status`: one JSON line.
///
/// # Errors
///
/// The device list cannot be read.
pub fn status_json() -> Result<String> {
    let mut v = win::Real.health()?.to_json(locate().as_deref());
    v["elevated"] = json!(elevation::is_elevated());
    Ok(v.to_string())
}

/// `vdd repair --dry-run`: the steps a repair would take now. Nothing is changed.
///
/// # Errors
///
/// The device list cannot be read, or the driver is not installed.
pub fn repair_plan(restart: Restart) -> Result<String> {
    let h = win::Real.health()?;
    let steps = plan(&h, restart)?;
    if steps.is_empty() {
        return Ok(format!("nothing to repair (cause: {})", h.cause().name()));
    }
    let admin = steps.iter().any(|s| s.needs_admin()) && !elevation::is_elevated();
    let mut lines = vec![format!("cause: {}", h.cause().name())];
    lines.extend(steps.iter().map(|s| format!("would {}", s.words())));
    lines.push(if admin { "one UAC prompt".to_string() } else { "no UAC prompt".to_string() });
    Ok(lines.join("\n"))
}

/// `vdd install --dry-run`: what an install would do now. Nothing is changed.
///
/// # Errors
///
/// The device list cannot be read, or the bundled installer is missing or changed.
pub fn install_plan(how: Reinstall) -> Result<String> {
    let h = win::Real.health()?;
    if how == Reinstall::IfMissing && h.adapter != Adapter::Missing {
        return Ok(format!(
            "driver {} is installed: no install needed",
            h.version.as_deref().unwrap_or("(no version)")
        ));
    }
    let exe = locate().context("the bundled Parsec installer was not found")?;
    check_installer(&exe)?;
    Ok(format!("would run {} /S (hash checked), then set the render values; one UAC prompt", exe.display()))
}

/// `vdd repair`. A step that needs no administrator shows no prompt.
///
/// # Errors
///
/// See [`repair`]; also the UAC prompt was declined.
pub fn repair_command(restart: Restart) -> Result<String> {
    let h = win::Real.health()?;
    let steps = plan(&h, restart)?;
    if !steps.iter().any(|s| s.needs_admin()) {
        return repair(&mut win::Real, restart);
    }
    let action = match restart {
        Restart::IfNeeded => "repair",
        Restart::Always => "repair-restart",
    };
    let done = elevated(action, Path::new("-"))?;
    if steps.contains(&Action::RestartAdapter) && h.render.nvidia_present {
        return confirm_render(done, &win::rendered_now());
    }
    Ok(done)
}

/// `vdd install`.
///
/// # Errors
///
/// No bundled installer, the UAC prompt was declined, or the install failed.
pub fn install_command(how: Reinstall) -> Result<String> {
    if how == Reinstall::IfMissing && win::Real.health()?.adapter != Adapter::Missing {
        return repair_command(Restart::IfNeeded);
    }
    let exe = locate().context("the bundled Parsec installer was not found")?;
    check_installer(&exe)?;
    let action = match how {
        Reinstall::IfMissing => "install",
        Reinstall::Force => "reinstall",
    };
    let done = elevated(action, &exe)?;
    if win::Real.health()?.render.nvidia_present {
        return confirm_render(done, &win::rendered_now());
    }
    Ok(done)
}

/// Run `action` elevated: directly when this process is, else this exe again with one UAC prompt
/// (`vdd elevated <action> <installer> <result file>`). Returns the child's message.
fn elevated(action: &str, installer: &Path) -> Result<String> {
    if elevation::is_elevated() {
        return run_action(action, installer);
    }
    let result = std::env::temp_dir().join(format!("tunnelrez-vdd-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&result);
    let exe = std::env::current_exe()?;
    let args = vec![
        "vdd".to_string(),
        "elevated".into(),
        action.into(),
        installer.display().to_string(),
        result.display().to_string(),
    ];
    let code = elevation::run_as_admin(&exe, &args, |_| {})?;
    let text = std::fs::read_to_string(&result).unwrap_or_default();
    let _ = std::fs::remove_file(&result);
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("no result").to_string();
    if code != 0 || v.get("ok") != Some(&Value::Bool(true)) {
        bail!("{msg} (exit code {code})");
    }
    Ok(msg)
}

/// The elevated work. It reads the health again: the PC may have changed during the prompt.
fn run_action(action: &str, installer: &Path) -> Result<String> {
    let mut fx = win::Real;
    match action {
        "repair" => repair(&mut fx, Restart::IfNeeded),
        "repair-restart" => repair(&mut fx, Restart::Always),
        "install" => install(&mut fx, installer, Reinstall::IfMissing),
        "reinstall" => install(&mut fx, installer, Reinstall::Force),
        x => bail!("unknown elevated action {x}"),
    }
}

/// The elevated child (`vdd elevated <action> <installer> <result file>`): do the work and write
/// `{"ok":bool,"msg":...}` to the result file (an elevated process's output cannot be read by its parent).
pub fn elevated_child(action: &str, installer: &str, result: &str) -> i32 {
    let r = run_action(action, Path::new(installer));
    let ok = r.is_ok();
    let msg = r.unwrap_or_else(|e| format!("{e:#}"));
    let _ = std::fs::write(result, json!({ "ok": ok, "msg": msg }).to_string());
    i32::from(!ok)
}

/// The real device, registry and installer calls.
mod win {
    use super::{
        Action, Adapter, Fixer, Health, Reboot, Render, Rendered, HARDWARE_ID, INSTALLER_TIMEOUT, VENDOR_NVIDIA,
    };
    use crate::audio::driver::win::multi_sz;
    use crate::tools::{Display, Tools};
    use anyhow::{bail, Context, Result};
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use windows::core::{GUID, HSTRING, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_Status, SetupDiCallClassInstaller, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
        SetupDiGetClassDevsW, SetupDiGetDeviceInstallParamsW, SetupDiGetDeviceInstanceIdW,
        SetupDiGetDeviceRegistryPropertyW, SetupDiSetClassInstallParamsW, CM_DEVNODE_STATUS_FLAGS, CM_PROB,
        CM_PROB_DISABLED, CR_SUCCESS, DICS_ENABLE, DICS_FLAG_GLOBAL, DICS_PROPCHANGE, DIF_PROPERTYCHANGE,
        DIGCF_PRESENT, DI_NEEDREBOOT, DI_NEEDRESTART, DN_HAS_PROBLEM, GUID_DEVCLASS_DISPLAY, GUID_DEVCLASS_MONITOR,
        HDEVINFO, SETUP_DI_GET_CLASS_DEVS_FLAGS, SPDRP_HARDWAREID, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA,
        SP_DEVINSTALL_PARAMS_W, SP_PROPCHANGE_PARAMS,
    };
    use windows::Win32::System::Registry::{
        RegGetValueW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    /// The driver's settings.
    const PARAMETERS: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\WUDF\Services\ParsecVDA\Parameters";
    /// The driver's Apps entry.
    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\ParsecVDD";
    const DISABLE_CHANGE: &str = "DisablePreferredRenderAdapterChange";
    const VENDOR_ID: &str = "PreferredRenderAdapterVendorId";
    /// What a Parsec monitor's device ID contains.
    const MONITOR_ID: &str = "PSCCDD0";
    /// Bytes for a hardware ID list.
    const ID_BYTES: usize = 2048;
    /// How many times (100 ms apart) the render check looks for its monitor.
    const RENDER_CHECK_TRIES: usize = 50;

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

    /// The devices of `class`, as (set, members).
    fn devices(class: &GUID, flags: SETUP_DI_GET_CLASS_DEVS_FLAGS) -> Result<(Set, Vec<SP_DEVINFO_DATA>)> {
        // SAFETY: a valid class GUID; returns an owned set (destroyed by `Set`).
        let set = Set(unsafe { SetupDiGetClassDevsW(Some(class), PCWSTR::null(), None, flags) }
            .context("SetupDiGetClassDevsW")?);
        let mut all = Vec::new();
        for i in 0.. {
            let mut d = data();
            // SAFETY: valid set and out-struct with cbSize set; fails at the end of the list.
            if unsafe { SetupDiEnumDeviceInfo(set.0, i, &raw mut d) }.is_err() {
                break;
            }
            all.push(d);
        }
        Ok((set, all))
    }

    fn hardware_ids(set: &Set, d: &SP_DEVINFO_DATA) -> Vec<String> {
        let mut buf = vec![0u8; ID_BYTES];
        let mut need = 0u32;
        // SAFETY: valid set and member; the buffer and size out-pointer are valid for the call.
        let got = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                std::ptr::from_ref(d),
                SPDRP_HARDWAREID,
                None,
                Some(&mut buf),
                Some(&raw mut need),
            )
        };
        if got.is_err() {
            return Vec::new();
        }
        multi_sz(&buf[..usize::try_from(need).unwrap_or(0).min(buf.len())])
    }

    fn instance_id(set: &Set, d: &SP_DEVINFO_DATA) -> String {
        let mut buf = [0u16; 256];
        // SAFETY: valid set and member; the buffer is valid for the call.
        if unsafe { SetupDiGetDeviceInstanceIdW(set.0, std::ptr::from_ref(d), Some(&mut buf), None) }.is_err() {
            return String::new();
        }
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// The node's problem code (0: none); `None` when the device is not present.
    fn problem(d: &SP_DEVINFO_DATA) -> Option<u32> {
        let mut status = CM_DEVNODE_STATUS_FLAGS(0);
        let mut code = CM_PROB(0);
        // SAFETY: valid out-pointers; a stale devinst only returns an error code.
        let r = unsafe { CM_Get_DevNode_Status(&raw mut status, &raw mut code, d.DevInst, 0) };
        (r == CR_SUCCESS).then_some(if (status.0 & DN_HAS_PROBLEM.0) != 0 { code.0 } else { 0 })
    }

    fn reg_dword(key: &str, value: &str) -> Option<u32> {
        let mut v = 0u32;
        let mut size = 4u32;
        // SAFETY: the strings live for the call; `v` and `size` are a valid 4-byte buffer and its size.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(key),
                &HSTRING::from(value),
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut v).cast()),
                Some(&raw mut size),
            )
        };
        r.is_ok().then_some(v)
    }

    fn reg_string(key: &str, value: &str) -> Option<String> {
        let mut buf = [0u16; 64];
        let mut size = u32::try_from(std::mem::size_of_val(&buf)).unwrap_or(0);
        // SAFETY: the strings live for the call; `buf` is `size` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(key),
                &HSTRING::from(value),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&raw mut size),
            )
        };
        if r.is_err() {
            return None;
        }
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    fn set_dword(value: &str, data: u32) -> Result<()> {
        let bytes = data.to_le_bytes();
        // SAFETY: the strings live for the call; `bytes` is the 4-byte value.
        unsafe {
            RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(PARAMETERS),
                &HSTRING::from(value),
                REG_DWORD.0,
                Some(bytes.as_ptr().cast()),
                u32::try_from(bytes.len()).unwrap_or(4),
            )
        }
        .ok()
        .with_context(|| format!("write {value}"))
    }

    /// Enable or restart the adapter.
    fn change_adapter(state: u32) -> Result<Reboot> {
        let (set, all) = devices(&GUID_DEVCLASS_DISPLAY, DIGCF_PRESENT)?;
        let Some(d) = all.iter().find(|d| is_parsec(&set, d)) else { bail!("no {HARDWARE_ID} adapter") };
        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: u32::try_from(std::mem::size_of::<SP_CLASSINSTALL_HEADER>()).unwrap_or(0),
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_STATE_CHANGE(state),
            Scope: DICS_FLAG_GLOBAL,
            HwProfile: 0,
        };
        let size = u32::try_from(std::mem::size_of::<SP_PROPCHANGE_PARAMS>()).unwrap_or(0);
        // SAFETY: valid set and member; `params` starts with its header and is `size` bytes.
        unsafe {
            SetupDiSetClassInstallParamsW(
                set.0,
                Some(std::ptr::from_ref(d)),
                Some(std::ptr::from_ref(&params).cast()),
                size,
            )
        }
        .context("SetupDiSetClassInstallParamsW")?;
        // SAFETY: valid set and member, with the install params set above.
        unsafe { SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set.0, Some(std::ptr::from_ref(d))) }
            .context("DIF_PROPERTYCHANGE")?;
        let mut install = SP_DEVINSTALL_PARAMS_W {
            cbSize: u32::try_from(std::mem::size_of::<SP_DEVINSTALL_PARAMS_W>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: valid set and member; `install` is a sized out-struct.
        unsafe { SetupDiGetDeviceInstallParamsW(set.0, Some(std::ptr::from_ref(d)), &raw mut install) }
            .context("SetupDiGetDeviceInstallParamsW")?;
        let wanted = DI_NEEDREBOOT.0 | DI_NEEDRESTART.0;
        Ok(if install.Flags.0 & wanted != 0 { Reboot::Needed } else { Reboot::No })
    }

    fn is_parsec(set: &Set, d: &SP_DEVINFO_DATA) -> bool {
        hardware_ids(set, d).iter().any(|h| h.eq_ignore_ascii_case(HARDWARE_ID))
    }

    /// Which GPU the TV monitor renders on now. With none on the desktop, one is plugged for a moment: the driver
    /// picks its GPU for a monitor, so nothing else can show it.
    pub(super) fn rendered_now() -> Rendered {
        let attached = tunnelrez_vdd::vdd_displays().into_iter().find(|d| d.attached).map(|d| d.name);
        let plugged = if attached.is_some() {
            None
        } else {
            match tunnelrez_vdd::VirtualDisplay::plug(tunnelrez_vdd::Mode { width: 1920, height: 1080, hz: 60 }) {
                Ok(d) => Some(d),
                Err(e) => return Rendered::Unknown(format!("the monitor did not plug: {e}")),
            }
        };
        let Some(name) = attached.or_else(|| plugged.as_ref().and_then(|d| d.device_name.clone())) else {
            return Rendered::Unknown("the monitor did not attach".into());
        };
        let tools = match Tools::setup(None) {
            Ok(t) => t,
            Err(e) => return Rendered::Unknown(format!("{e:#}")),
        };
        // DXGI sees a new monitor a little after GDI.
        for _ in 0..RENDER_CHECK_TRIES {
            if let Ok(views) = tools.views() {
                if let Some(m) = views.render_adapter(&name, Display::Virtual) {
                    return if m.vendor_id == crate::monitors::VENDOR_NVIDIA {
                        Rendered::Nvidia
                    } else {
                        Rendered::Other(m.adapter_name.clone())
                    };
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Rendered::Unknown(format!("{name} is not visible to DXGI"))
    }

    /// The real PC.
    #[derive(Debug)]
    pub(super) struct Real;

    impl Fixer for Real {
        fn health(&mut self) -> Result<Health> {
            let (set, gpus) = devices(&GUID_DEVCLASS_DISPLAY, DIGCF_PRESENT)?;
            let adapter = gpus.iter().find(|d| is_parsec(&set, d)).map_or(Adapter::Missing, |d| match problem(d) {
                Some(0) => Adapter::Running,
                Some(c) if c == CM_PROB_DISABLED.0 => Adapter::Disabled,
                Some(code) => Adapter::Failed { code },
                None => Adapter::Missing,
            });
            let nvidia_present =
                gpus.iter().any(|d| hardware_ids(&set, d).iter().any(|h| h.to_ascii_uppercase().contains("VEN_10DE")));
            // Every device, present or not: the ones Windows only remembers are the phantoms.
            let (mons, all) = devices(&GUID_DEVCLASS_MONITOR, SETUP_DI_GET_CLASS_DEVS_FLAGS(0))?;
            let ours: Vec<_> = all.iter().filter(|d| instance_id(&mons, d).contains(MONITOR_ID)).collect();
            let phantom = ours.iter().filter(|d| problem(d).is_none()).count();
            let failed_monitors = ours.iter().filter(|d| problem(d).is_some_and(|c| c != 0)).count();
            let displays = tunnelrez_vdd::vdd_displays();
            Ok(Health {
                version: reg_string(UNINSTALL, "DisplayVersion"),
                adapter,
                render: Render {
                    nvidia_present,
                    disable_change: reg_dword(PARAMETERS, DISABLE_CHANGE),
                    vendor_id: reg_dword(PARAMETERS, VENDOR_ID),
                },
                attached: displays.iter().filter(|d| d.attached).map(|d| d.name.clone()).collect(),
                detached: displays.iter().any(|d| !d.attached),
                phantom,
                failed_monitors,
            })
        }

        fn act(&mut self, action: Action) -> Result<Reboot> {
            match action {
                Action::EnableAdapter => change_adapter(DICS_ENABLE.0),
                Action::RestartAdapter => change_adapter(DICS_PROPCHANGE.0),
                Action::SetRender => {
                    set_dword(DISABLE_CHANGE, 0)?;
                    set_dword(VENDOR_ID, VENDOR_NVIDIA)?;
                    Ok(Reboot::No)
                }
                Action::ClearPlug => {
                    tunnelrez_vdd::unplug_all()?;
                    Ok(Reboot::No)
                }
            }
        }

        fn run_installer(&mut self, exe: &Path) -> Result<u32> {
            const NO_WINDOW: u32 = 0x0800_0000;
            let mut child = std::process::Command::new(exe)
                .arg("/S")
                .creation_flags(NO_WINDOW)
                .spawn()
                .with_context(|| format!("run {}", exe.display()))?;
            let t0 = std::time::Instant::now();
            loop {
                if let Some(s) = child.try_wait().context("wait for the installer")? {
                    return Ok(s.code().and_then(|c| u32::try_from(c).ok()).unwrap_or(1));
                }
                if t0.elapsed() >= INSTALLER_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("the installer took longer than {} s", INSTALLER_TIMEOUT.as_secs());
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Health {
        Health {
            version: Some(VERSION.into()),
            adapter: Adapter::Running,
            render: Render { nvidia_present: true, disable_change: Some(0), vendor_id: Some(VENDOR_NVIDIA) },
            attached: Vec::new(),
            detached: false,
            phantom: 2,
            failed_monitors: 0,
        }
    }

    /// A fake PC: each action changes what the next `health` reads, and is recorded.
    struct Fake {
        h: Health,
        calls: Vec<String>,
        /// The step that Windows refuses.
        refuse: Option<Action>,
        /// The installer's exit code, and whether it brings the adapter.
        installer: (u32, bool),
        /// The step that Windows accepts but wants a restart for.
        reboot: Option<Action>,
    }

    fn fake(h: Health) -> Fake {
        Fake { h, calls: Vec::new(), refuse: None, installer: (0, true), reboot: None }
    }

    impl Fixer for Fake {
        fn health(&mut self) -> Result<Health> {
            Ok(self.h.clone())
        }
        fn act(&mut self, a: Action) -> Result<Reboot> {
            self.calls.push(a.words().into());
            if self.refuse == Some(a) {
                bail!("refused");
            }
            match a {
                Action::EnableAdapter | Action::RestartAdapter => {
                    self.h.adapter = Adapter::Running;
                    self.h.failed_monitors = 0;
                    self.h.detached = false;
                    self.h.attached.clear();
                }
                Action::SetRender => {
                    self.h.render.disable_change = Some(0);
                    self.h.render.vendor_id = Some(VENDOR_NVIDIA);
                }
                Action::ClearPlug => self.h.detached = false,
            }
            Ok(if self.reboot == Some(a) { Reboot::Needed } else { Reboot::No })
        }
        fn run_installer(&mut self, _: &Path) -> Result<u32> {
            self.calls.push("installer".into());
            if self.installer.1 {
                self.h.adapter = Adapter::Running;
                self.h.version = Some(VERSION.into());
            }
            Ok(self.installer.0)
        }
    }

    #[test]
    fn this_pc_reads_healthy() {
        let h = healthy();
        assert_eq!(h.cause(), Cause::None);
        assert!(plan(&h, Restart::IfNeeded).unwrap().is_empty(), "phantom monitors are normal");
    }

    #[test]
    fn the_real_cause_comes_first() {
        let c = |f: fn(&mut Health)| {
            let mut h = healthy();
            f(&mut h);
            h.cause()
        };
        assert_eq!(c(|h| h.adapter = Adapter::Missing), Cause::Missing);
        assert_eq!(c(|h| h.adapter = Adapter::Disabled), Cause::Disabled);
        assert_eq!(c(|h| h.adapter = Adapter::Failed { code: 43 }), Cause::AdapterProblem);
        assert_eq!(c(|h| h.adapter = Adapter::Failed { code: 14 }), Cause::RestartWindows);
        assert_eq!(c(|h| h.failed_monitors = 1), Cause::AdapterProblem);
        assert_eq!(c(|h| h.render.vendor_id = None), Cause::Render);
        assert_eq!(c(|h| h.detached = true), Cause::Stuck);
    }

    #[test]
    fn an_attached_monitor_is_not_stuck() {
        let h = Health { attached: vec![r"\\.\DISPLAY10".into()], detached: true, ..healthy() };
        assert_eq!(h.cause(), Cause::None, "a running stream is left alone");
        assert!(plan(&h, Restart::IfNeeded).unwrap().is_empty());
    }

    #[test]
    fn the_render_values_matter_only_with_an_nvidia_gpu() {
        let h = Health { render: Render::default(), ..healthy() };
        assert_eq!(h.cause(), Cause::None);
        assert!(plan(&h, Restart::IfNeeded).unwrap().is_empty());
        let wrong = Render { nvidia_present: true, disable_change: Some(1), vendor_id: Some(VENDOR_NVIDIA) };
        assert!(!wrong.ok(), "changes disabled: the vendor value is not used");
    }

    #[test]
    fn repair_does_only_what_is_needed() {
        let mut f = fake(Health { adapter: Adapter::Disabled, ..healthy() });
        repair(&mut f, Restart::IfNeeded).unwrap();
        assert_eq!(f.calls, ["enable the adapter"]);

        let bad = Render { nvidia_present: true, disable_change: None, vendor_id: None };
        let mut f = fake(Health { render: bad, ..healthy() });
        repair(&mut f, Restart::IfNeeded).unwrap();
        assert_eq!(
            f.calls,
            ["set the render-on-NVIDIA values", "restart the adapter"],
            "the driver reads them at start"
        );

        let mut f = fake(Health { detached: true, ..healthy() });
        repair(&mut f, Restart::IfNeeded).unwrap();
        assert_eq!(f.calls, ["clear the stuck plug"]);

        let mut f = fake(Health { adapter: Adapter::Failed { code: 43 }, render: bad, ..healthy() });
        repair(&mut f, Restart::IfNeeded).unwrap();
        assert_eq!(f.calls, ["set the render-on-NVIDIA values", "restart the adapter"], "the values first");
    }

    #[test]
    fn repair_is_idempotent() {
        let mut f = fake(Health { adapter: Adapter::Disabled, ..healthy() });
        repair(&mut f, Restart::IfNeeded).unwrap();
        let n = f.calls.len();
        assert!(repair(&mut f, Restart::IfNeeded).unwrap().contains("nothing to repair"));
        assert_eq!(f.calls.len(), n, "the second run changes nothing");
    }

    #[test]
    fn only_the_plug_needs_no_prompt() {
        assert!(!Action::ClearPlug.needs_admin());
        assert!(Action::EnableAdapter.needs_admin() && Action::RestartAdapter.needs_admin());
        assert!(Action::SetRender.needs_admin());
    }

    #[test]
    fn a_forced_restart_restarts_a_healthy_adapter() {
        let mut f = fake(healthy());
        repair(&mut f, Restart::Always).unwrap();
        assert_eq!(f.calls, ["restart the adapter"]);
    }

    #[test]
    fn a_restart_of_windows_that_only_setup_reports_is_kept() {
        // The adapter restart succeeds and the next health read looks fine: Windows' flag is the only sign.
        let mut f = fake(Health { adapter: Adapter::Failed { code: 43 }, ..healthy() });
        f.reboot = Some(Action::RestartAdapter);
        let out = repair(&mut f, Restart::IfNeeded).unwrap();
        assert!(out.contains("REBOOT_REQUIRED"), "{out}");
        // ... and it survives a later step that wants none.
        let bad = Render { nvidia_present: true, disable_change: None, vendor_id: None };
        let mut f = fake(Health { adapter: Adapter::Disabled, render: bad, ..healthy() });
        f.reboot = Some(Action::EnableAdapter);
        let out = repair(&mut f, Restart::IfNeeded).unwrap();
        assert_eq!(f.calls, ["enable the adapter", "set the render-on-NVIDIA values", "restart the adapter"]);
        assert!(out.contains("REBOOT_REQUIRED"), "{out}");
    }

    #[test]
    fn a_new_render_value_waits_for_nothing_but_a_restart_of_windows() {
        let bad = Render { nvidia_present: true, disable_change: None, vendor_id: None };
        let h = Health { adapter: Adapter::Failed { code: 14 }, render: bad, ..healthy() };
        assert_eq!(plan(&h, Restart::IfNeeded).unwrap(), [Action::SetRender], "Windows restarts it anyway");
    }

    #[test]
    fn repair_reports_where_the_monitor_really_renders() {
        let done = || "restart the adapter".to_string();
        assert!(confirm_render(done(), &Rendered::Nvidia).unwrap().contains("renders on NVIDIA"));
        let e = confirm_render(done(), &Rendered::Other("Intel(R) Graphics".into())).unwrap_err().to_string();
        assert!(e.contains(RENDER_STUCK) && e.contains("Intel(R) Graphics") && e.contains("Restart Windows"), "{e}");
        let out = confirm_render(done(), &Rendered::Unknown("the monitor did not attach".into())).unwrap();
        assert!(out.contains("could not be checked"), "never claims success: {out}");
        // Windows must restart first: the GPU says nothing yet.
        let reboot = "x. Restart Windows to finish (REBOOT_REQUIRED)".to_string();
        assert_eq!(confirm_render(reboot.clone(), &Rendered::Other("Intel".into())).unwrap(), reboot);
    }

    #[test]
    fn repair_says_when_it_did_not_help() {
        let mut f = fake(Health { adapter: Adapter::Disabled, ..healthy() });
        f.refuse = Some(Action::EnableAdapter);
        let e = format!("{:#}", repair(&mut f, Restart::IfNeeded).unwrap_err());
        assert!(e.contains("could not enable the adapter"), "{e}");
    }

    #[test]
    fn repair_points_a_missing_driver_at_install() {
        let mut f = fake(Health { adapter: Adapter::Missing, version: None, ..healthy() });
        let e = repair(&mut f, Restart::IfNeeded).unwrap_err().to_string();
        assert!(e.contains("not installed"), "{e}");
        assert!(f.calls.is_empty());
    }

    #[test]
    fn a_restart_of_windows_is_not_a_repair() {
        let mut f = fake(Health { adapter: Adapter::Failed { code: 14 }, ..healthy() });
        assert!(repair(&mut f, Restart::IfNeeded).unwrap().contains("nothing to repair"));
        assert!(f.calls.is_empty(), "restarting the adapter does not replace the restart");
    }

    /// The real bundled installer, so `check_installer` passes.
    fn bundled() -> PathBuf {
        BUNDLED.iter().fold(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."), |p, c| p.join(c))
    }

    #[test]
    fn install_runs_the_installer_then_sets_the_values() {
        let bad = Render { nvidia_present: true, disable_change: None, vendor_id: None };
        let mut f = fake(Health { adapter: Adapter::Missing, version: None, render: bad, ..healthy() });
        let out = install(&mut f, &bundled(), Reinstall::IfMissing).unwrap();
        let want = ["installer", "set the render-on-NVIDIA values", "restart the adapter"];
        assert_eq!(f.calls, want, "one prompt covers all, and the driver reads the values when it starts");
        assert!(out.contains("installed"), "{out}");
    }

    #[test]
    fn install_is_idempotent() {
        let mut f = fake(healthy());
        assert!(install(&mut f, &bundled(), Reinstall::IfMissing).unwrap().contains("nothing to repair"));
        assert!(f.calls.is_empty());
        install(&mut f, &bundled(), Reinstall::Force).unwrap();
        assert_eq!(f.calls, ["installer"], "--force runs it anyway");
    }

    #[test]
    fn install_reports_a_failed_installer() {
        let mut f = fake(Health { adapter: Adapter::Missing, version: None, ..healthy() });
        f.installer = (2, false);
        let e = install(&mut f, &bundled(), Reinstall::IfMissing).unwrap_err().to_string();
        assert!(e.contains("exit code 2"), "{e}");
        f.installer = (0, false);
        let e = install(&mut f, &bundled(), Reinstall::IfMissing).unwrap_err().to_string();
        assert!(e.contains("no Root"), "exit 0 without an adapter is no success: {e}");
    }

    #[test]
    fn install_asks_for_a_restart_of_windows_when_the_adapter_needs_it() {
        // The installer exits 0 and the adapter comes up waiting for a restart.
        struct Late(Fake);
        impl Fixer for Late {
            fn health(&mut self) -> Result<Health> {
                self.0.health()
            }
            fn act(&mut self, a: Action) -> Result<Reboot> {
                self.0.act(a)
            }
            fn run_installer(&mut self, e: &Path) -> Result<u32> {
                self.0.h.adapter = Adapter::Failed { code: PROBLEM_NEED_RESTART };
                self.0.run_installer(e)
            }
        }
        let mut f = fake(Health { adapter: Adapter::Missing, version: None, ..healthy() });
        f.installer = (0, false);
        let out = install(&mut Late(f), &bundled(), Reinstall::IfMissing).unwrap();
        assert!(out.contains("REBOOT_REQUIRED"), "{out}");
    }

    #[test]
    fn a_changed_installer_never_runs() {
        let dir = std::env::temp_dir().join(format!("tunnelrez-vdd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("evil.exe");
        std::fs::write(&exe, b"not the installer").unwrap();
        let mut f = fake(Health { adapter: Adapter::Missing, version: None, ..healthy() });
        let e = install(&mut f, &exe, Reinstall::IfMissing).unwrap_err().to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(e.contains("not the Parsec installer"), "{e}");
        assert!(f.calls.is_empty(), "{:?}", f.calls);
    }

    #[test]
    fn the_installer_is_found_beside_the_exe_or_in_the_checkout() {
        let kit = Path::new(r"C:\kit");
        let want = BUNDLED.iter().fold(kit.to_path_buf(), |p, c| p.join(c));
        assert_eq!(find_installer(kit, |p| p == want.as_path()), Some(want));
        let repo = Path::new(r"C:\repo");
        let want = BUNDLED.iter().fold(repo.to_path_buf(), |p, c| p.join(c));
        assert_eq!(find_installer(&repo.join(r"sender\target\release"), |p| p == want.as_path()), Some(want));
        assert_eq!(find_installer(kit, |_| false), None);
    }

    /// The bundled installer is the unchanged original (`drivers/parsec-vdd/SHA256SUMS`, and the hash in this file).
    #[test]
    fn bundled_installer_matches_its_hash() {
        check_installer(&bundled()).unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\drivers\parsec-vdd");
        let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).expect("SHA256SUMS");
        let (want, file) = sums.trim().split_once("  ").expect("sha256sum format");
        assert_eq!(want, INSTALLER_SHA256);
        assert_eq!(file, BUNDLED[2]);
    }

    #[test]
    fn status_json_has_the_fields_the_app_reads() {
        let v = healthy().to_json(Some(Path::new("x.exe")));
        assert_eq!(v["driver_installed"], true);
        assert_eq!(v["driver_version"], VERSION);
        assert_eq!(v["adapter"]["state"], "ok");
        assert_eq!(v["render_on_nvidia"]["ok"], true);
        assert_eq!(v["monitors"]["phantom"], 2);
        assert_eq!(
            (v["ready"].as_bool(), v["cause"].as_str(), v["next"].as_str()),
            (Some(true), Some("none"), Some("none"))
        );
        let v = Health { adapter: Adapter::Missing, version: None, ..healthy() }.to_json(None);
        assert_eq!((v["driver_installed"].as_bool(), v["next"].as_str()), (Some(false), Some("install")));
        let v = Health { adapter: Adapter::Failed { code: 43 }, ..healthy() }.to_json(None);
        assert_eq!((v["adapter"]["state"].as_str(), v["adapter"]["problem"].as_u64()), (Some("problem"), Some(43)));
    }
}
