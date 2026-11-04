//! Everything Windows-specific: child-process control, the virtual monitor, display queries, the HKCU Run key,
//! theme colors from the registry, flyout geometry, and the logoff/shutdown hook.
//!
//! The rest of the app talks to this module only through the plain types and functions re-exported by
//! `platform/mod.rs`, so a macOS/Linux port adds a sibling file instead of touching the app logic.

use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};

use ::windows::core::{w, BOOL, PCWSTR};
use ::windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use ::windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use ::windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromPoint, DEVMODEW, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use ::windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
};
use ::windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use ::windows::Win32::UI::HiDpi::GetDpiForSystem;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG,
    WINDOW_EX_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};
use anyhow::{anyhow, Context, Result};

// ------------------------------------------------------------------------------------------------ processes

/// `CREATE_NO_WINDOW`: a console child gets a console, but no window ever appears.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `CREATE_NEW_PROCESS_GROUP`: the child leads its own group, so a Ctrl+Break can be aimed at it alone.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Make `cmd` start without a console window. Use for every helper process the app runs.
pub(crate) fn hide_console(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

/// Open a URI (`ms-windows-store://...`) with its registered handler.
///
/// # Errors
/// When Explorer cannot be started.
pub(crate) fn open_uri(uri: &str) -> anyhow::Result<()> {
    // `rundll32 url.dll,FileProtocolHandler` fails (non-zero exit) when no app handles the scheme, so the caller
    // can fall back to the web page.
    let st = hide_console(&mut Command::new("rundll32.exe")).args(["url.dll,FileProtocolHandler", uri]).status()?;
    anyhow::ensure!(st.success(), "no handler for {uri}");
    Ok(())
}

/// Open a folder in Explorer. Does not wait (Explorer's exit code means nothing).
///
/// # Errors
/// When Explorer cannot be started.
pub(crate) fn open_folder(dir: &Path) -> anyhow::Result<()> {
    Command::new("explorer.exe").arg(dir).spawn().context("start Explorer")?;
    Ok(())
}

/// Like [`hide_console`], and the child leads its own process group so [`send_ctrl_break`] can stop it cleanly.
pub(crate) fn hide_console_own_group(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
}

/// A job object that kills every process in it when the last handle closes, i.e. when this app exits or crashes.
/// Processes the sender starts (ffmpeg) join the job too, so nothing is ever left running.
#[derive(Debug)]
pub(crate) struct Job(HANDLE);

// SAFETY: a job handle is a process-wide kernel handle. The only calls made on it (AssignProcessToJobObject,
// TerminateJobObject, CloseHandle) are thread-safe kernel calls, and the handle is closed exactly once in Drop.
unsafe impl Send for Job {}
// SAFETY: see Send; `&Job` only ever passes the handle value to thread-safe kernel calls.
unsafe impl Sync for Job {}

impl Job {
    /// # Errors
    /// When the job cannot be created or configured.
    pub(crate) fn new() -> Result<Self> {
        // SAFETY: no security attributes and no name; the returned handle is owned by `Job` and closed in Drop.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("create job object")?;
        let job = Job(handle);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).expect("small struct");
        // SAFETY: `info` is a fully initialised JOBOBJECT_EXTENDED_LIMIT_INFORMATION and `size` is its size; the
        // kernel only reads it during the call.
        unsafe {
            SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, std::ptr::from_ref(&info).cast(), size)
        }
        .context("configure job object")?;
        Ok(job)
    }

    /// # Errors
    /// When the process cannot join the job (it already exited, or it is in a job that forbids nesting).
    pub(crate) fn assign(&self, child: &Child) -> Result<()> {
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: both handles are valid: `self.0` until Drop, `process` while `child` is borrowed.
        unsafe { AssignProcessToJobObject(self.0, process) }.context("assign process to job")
    }

    /// Kill every process in the job now. Does not block.
    pub(crate) fn terminate(&self) {
        // SAFETY: `self.0` is a valid job handle until Drop. Failure (job already empty) needs no handling.
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed only here. Closing the last handle kills the
        // processes in the job (KILL_ON_JOB_CLOSE), which is the intent.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Console attach/detach is process-wide state: only one thread may do it at a time.
static CONSOLE: Mutex<()> = Mutex::new(());

/// Swallows Ctrl+C / Ctrl+Break while this GUI process is briefly attached to a child's console.
unsafe extern "system" fn swallow_ctrl(kind: u32) -> BOOL {
    BOOL::from(kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT)
}

/// Ask a console child (started with [`hide_console_own_group`]) to stop, as if Ctrl+Break was pressed in its
/// console. The sender treats it like Ctrl+C: it sends BYE to the TV, stops its encoder and exits.
///
/// # Errors
/// When the child has no console to attach to, or the event cannot be sent.
pub(crate) fn send_ctrl_break(pid: u32) -> Result<()> {
    let _guard = CONSOLE.lock().expect("console lock poisoned");
    // SAFETY: a GUI process has no console of its own; FreeConsole on none is a harmless no-op.
    let _ = unsafe { FreeConsole() };
    // SAFETY: attaching to another process's console is allowed for any process we can open; failure is reported.
    unsafe { AttachConsole(pid) }.with_context(|| format!("attach to the console of process {pid}"))?;
    // SAFETY: `swallow_ctrl` is a valid `extern "system"` handler with static lifetime. Adding it twice is harmless.
    let _ = unsafe { SetConsoleCtrlHandler(Some(swallow_ctrl), true) };
    // SAFETY: the target is a process group id (the child was created with CREATE_NEW_PROCESS_GROUP, so its pid is
    // its group id); this process is not in that group, and `swallow_ctrl` covers it anyway.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    // SAFETY: detach from the child's console; it keeps running with its own console.
    let _ = unsafe { FreeConsole() };
    sent.context("send Ctrl+Break")
}

/// Find an executable on PATH.
pub(crate) fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

// ------------------------------------------------------------------------------------------------ registry

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("TunnelRez");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn reg_dword(root: HKEY, key: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: `data` is a 4-byte buffer and `size` says so; the strings are static NUL-terminated wide strings.
    let r = unsafe {
        RegGetValueW(
            root,
            key,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut data).cast()),
            Some(&raw mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(data)
}

fn reg_binary(root: HKEY, key: PCWSTR, value: PCWSTR, buf: &mut [u8]) -> Option<usize> {
    let mut size = u32::try_from(buf.len()).ok()?;
    // SAFETY: `buf` is writable for `size` bytes; the strings are static NUL-terminated wide strings.
    let r = unsafe {
        RegGetValueW(root, key, value, RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr().cast()), Some(&raw mut size))
    };
    if r == ERROR_SUCCESS {
        usize::try_from(size).ok()
    } else {
        None
    }
}

/// Whether `TunnelRez` starts with Windows (a value under `HKCU\...\Run`).
pub(crate) fn autostart_enabled() -> bool {
    let mut size = 0u32;
    // SAFETY: size query only (no data buffer); the strings are static NUL-terminated wide strings.
    let r =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&raw mut size)) };
    r == ERROR_SUCCESS
}

/// Whether "Start with Windows" is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Autostart {
    On,
    Off,
}

impl From<bool> for Autostart {
    /// `On` for true, `Off` for false.
    fn from(enabled: bool) -> Self {
        if enabled {
            Self::On
        } else {
            Self::Off
        }
    }
}

/// The Run value the app used before the rename.
const LEGACY_RUN_VALUE: PCWSTR = w!("SlipStream");

/// The exe a `Run` command line starts: the quoted first part (`"C:\x\tunnelrez.exe" --autostart`), else the text
/// up to the first space.
fn run_target(cmd: &str) -> &str {
    let cmd = cmd.trim();
    match cmd.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => cmd.split(' ').next().unwrap_or(cmd),
    }
}

/// The `TunnelRez` Run value, when there is one.
fn run_value() -> Option<String> {
    let mut buf = [0u16; 2048];
    let mut size = u32::try_from(std::mem::size_of_val(&buf)).ok()?;
    // SAFETY: `buf` is writable for `size` bytes; the strings are static NUL-terminated wide strings.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// "Start with Windows" set by the app before the rename (the `SlipStream` value, pointing at the old exe) becomes
/// the `TunnelRez` value pointing at this exe. An enabled `TunnelRez` value whose exe is gone (the app moved, or an
/// upgrade went to another folder) is pointed at this exe. A value that starts an exe that exists is left alone.
pub(crate) fn migrate_autostart() {
    if let Some(cmd) = run_value() {
        if !Path::new(run_target(&cmd)).is_file() {
            let set =
                std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| set_autostart(&exe, Autostart::On));
            if let Err(e) = set {
                crate::core::log(&format!("could not point Start with Windows at this exe: {e:#}"));
            }
        }
    }
    let mut size = 0u32;
    // SAFETY: size query only (no data buffer); the strings are static NUL-terminated wide strings.
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, LEGACY_RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&raw mut size))
    };
    if r != ERROR_SUCCESS {
        return;
    }
    let set = std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| set_autostart(&exe, Autostart::On));
    match set {
        Ok(()) => {
            // SAFETY: static NUL-terminated wide strings.
            let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, LEGACY_RUN_VALUE) };
            crate::core::log("Start with Windows moved from SlipStream to TunnelRez");
        }
        Err(e) => crate::core::log(&format!("could not move Start with Windows from SlipStream: {e:#}")),
    }
}

/// Turn "Start with Windows" on (pointing at `exe`, started with `--autostart`) or off.
///
/// # Errors
/// When the Run key cannot be written.
pub(crate) fn set_autostart(exe: &Path, want: Autostart) -> Result<()> {
    if want == Autostart::On {
        let cmd = wide(&format!("\"{}\" --autostart", exe.display()));
        let bytes = u32::try_from(cmd.len() * 2).context("path too long")?;
        // SAFETY: `cmd` is a NUL-terminated UTF-16 buffer of `bytes` bytes that outlives the call.
        let r = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(cmd.as_ptr().cast()), bytes)
        };
        if r != ERROR_SUCCESS {
            return Err(anyhow!("could not write the Run key (error {})", r.0));
        }
    } else if autostart_enabled() {
        // SAFETY: static NUL-terminated wide strings.
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
        if r != ERROR_SUCCESS {
            return Err(anyhow!("could not remove the Run key value (error {})", r.0));
        }
    }
    Ok(())
}

/// The Windows accent palette (`#rrggbb`), lightest first: Light3, Light2, Light1, Base, Dark1, Dark2, Dark3.
/// Win11 controls use Light2 in dark mode and Dark1 in light mode.
pub(crate) fn accent_palette() -> Option<Vec<String>> {
    let mut buf = [0u8; 32];
    let n = reg_binary(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent"),
        w!("AccentPalette"),
        &mut buf,
    )?;
    (n >= 28).then(|| buf[..28].chunks(4).map(|c| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])).collect())
}

/// Whether the taskbar (and so the tray) uses the light theme.
pub(crate) fn taskbar_is_light() -> bool {
    reg_dword(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("SystemUsesLightTheme"),
    )
    .is_some_and(|v| v != 0)
}

// ------------------------------------------------------------------------------------------------ geometry

/// A rectangle in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

/// The monitor rectangle and work area (monitor minus taskbar) of the monitor nearest to a point.
pub(crate) fn monitor_at(x: i32, y: i32) -> Option<(Rect, Rect)> {
    // SAFETY: plain value call; MONITOR_DEFAULTTONEAREST always yields a monitor.
    let hmon = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO { cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).ok()?, ..Default::default() };
    // SAFETY: `mi` is a MONITORINFO with cbSize set, writable for the call.
    unsafe { GetMonitorInfoW(hmon, &raw mut mi) }.ok().ok()?;
    let r = |r: ::windows::Win32::Foundation::RECT| Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
    Some((r(mi.rcMonitor), r(mi.rcWork)))
}

/// The tray icon size Windows uses at the system DPI: 16 px at 100 %, 20 at 125 %, 24 at 150 %, 32 at 200 %.
pub(crate) fn tray_icon_px() -> u32 {
    // SAFETY: no arguments; always succeeds.
    let dpi = unsafe { GetDpiForSystem() };
    match dpi {
        0..=96 => 16,
        97..=120 => 20,
        121..=144 => 24,
        _ => 32,
    }
}

// ------------------------------------------------------------------------------------------------ displays

/// What Windows knows about one active display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayName {
    /// The monitor's own name ("LG ULTRAGEAR"); may be empty.
    pub(crate) friendly: String,
    /// A built-in laptop panel.
    pub(crate) internal: bool,
}

/// A `u32` count from the Win32 API as a length. `usize` is at least 32 bits wide on every Windows target.
fn count(n: u32) -> usize {
    usize::try_from(n).expect("usize holds a u32 on Windows")
}

fn utf16_until_nul(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Friendly names of the active displays, keyed by GDI device name (`\\.\DISPLAY1`).
pub(crate) fn display_names() -> HashMap<String, DisplayName> {
    let mut out = HashMap::new();
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: both out-pointers are valid u32s.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &raw mut np, &raw mut nm) } != ERROR_SUCCESS {
        return out;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); count(np)];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); count(nm)];
    // SAFETY: the arrays have exactly the element counts passed in `np` / `nm`; the call may lower the counts.
    let r = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut np,
            paths.as_mut_ptr(),
            &raw mut nm,
            modes.as_mut_ptr(),
            None,
        )
    };
    if r != ERROR_SUCCESS {
        return out;
    }
    paths.truncate(count(np));
    for p in &paths {
        let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        src.header.size = u32::try_from(std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).expect("small struct");
        src.header.adapterId = p.sourceInfo.adapterId;
        src.header.id = p.sourceInfo.id;
        // SAFETY: `src` starts with a header whose type/size match the struct, as the API requires.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut src.header) } != 0 {
            continue;
        }
        let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        tgt.header.size = u32::try_from(std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>()).expect("small struct");
        tgt.header.adapterId = p.targetInfo.adapterId;
        tgt.header.id = p.targetInfo.id;
        // SAFETY: as above, for the target-name request.
        let ok = unsafe { DisplayConfigGetDeviceInfo(&raw mut tgt.header) } == 0;
        let tech = p.targetInfo.outputTechnology;
        let internal = tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
            || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
            || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
            || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS;
        let friendly = if ok { utf16_until_nul(&tgt.monitorFriendlyDeviceName) } else { String::new() };
        out.insert(utf16_until_nul(&src.viewGdiDeviceName), DisplayName { friendly, internal });
    }
    out
}

fn devmode(name: &[u16], index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let mut dm = DEVMODEW { dmSize: u16::try_from(std::mem::size_of::<DEVMODEW>()).ok()?, ..Default::default() };
    // SAFETY: `name` is NUL-terminated and `dm` has dmSize set.
    unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), index, &raw mut dm) }.as_bool().then_some(dm)
}

/// The current refresh rate of `gdi_name`.
pub(crate) fn display_refresh(gdi_name: &str) -> Option<u32> {
    devmode(&wide(gdi_name), ENUM_CURRENT_SETTINGS).map(|dm| dm.dmDisplayFrequency)
}

/// Where display `gdi_name` is on the desktop, physical px, from its current mode.
pub(crate) fn display_rect(gdi_name: &str) -> Option<Rect> {
    let dm = devmode(&wide(gdi_name), ENUM_CURRENT_SETTINGS)?;
    // SAFETY: EnumDisplaySettingsW fills the display arm of the union (dmPosition) for display devices.
    let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    let (w, h) = (i32::try_from(dm.dmPelsWidth).ok()?, i32::try_from(dm.dmPelsHeight).ok()?);
    (w > 0 && h > 0).then_some(Rect { left: pos.x, top: pos.y, right: pos.x + w, bottom: pos.y + h })
}

/// The screen the test page may cover: `gdi_name` only if it is a Parsec virtual display (the TV) and not the
/// main screen (the main screen is the one at the desktop origin). `None` otherwise: the page never opens on
/// the user's own screens.
pub(crate) fn test_screen(gdi_name: &str) -> Option<Rect> {
    if !virtual_display_names().iter().any(|n| n.eq_ignore_ascii_case(gdi_name)) {
        return None;
    }
    display_rect(gdi_name).filter(|r| (r.left, r.top) != (0, 0))
}

// ------------------------------------------------------------------------------------------------ virtual monitor

/// Who owns the virtual monitor the TV shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reuse {
    /// Use a Parsec virtual display that is already plugged (by anyone) instead of plugging a second one.
    Existing,
    /// Always plug our own (testing: `TUNNELREZ_VDD_PLUG=1`).
    PlugOwn,
}

/// The virtual monitor for Extend mode. Dropping an owned one unplugs it; a borrowed one is left alone.
pub(crate) enum VirtualMonitor {
    /// `_display` is held only for its Drop, which unplugs the monitor. `guard` unplugs it if this app dies.
    Owned {
        _display: tunnelrez_vdd::VirtualDisplay,
        name: String,
        guard: Option<Child>,
    },
    Borrowed {
        name: String,
    },
}

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        // Stop the guard first (a kill does not block), then the `_display` field unplugs the monitor.
        if let Self::Owned { guard: Some(g), .. } = self {
            let _ = g.kill();
            let _ = g.wait();
        }
    }
}

/// Start `this.exe --vdd-guard <our pid> <index>`: it waits for this process to end, then unplugs display
/// `index`. Needed because the driver keeps every display alive while *any* process pings it, so with another
/// virtual-display tool running, a crashed app would leave its monitor behind.
fn spawn_guard(index: u32) -> Option<Child> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    hide_console(&mut cmd)
        .args(["--vdd-guard", &std::process::id().to_string(), &index.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

/// The `--vdd-guard <pid> <index>` mode: wait for process `pid` to end, then unplug display `index`.
pub(crate) fn run_vdd_guard(pid: u32, index: u32) {
    use ::windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
    // SAFETY: opening another process only to wait on it; failure (it is already gone) falls through to unplug.
    if let Ok(h) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        // SAFETY: `h` is a valid process handle owned here.
        unsafe { WaitForSingleObject(h, INFINITE) };
        // SAFETY: closed once, right after the wait.
        let _ = unsafe { CloseHandle(h) };
    }
    let _ = tunnelrez_vdd::unplug_index(index);
}

impl std::fmt::Debug for VirtualMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned { name, .. } => write!(f, "VirtualMonitor::Owned({name})"),
            Self::Borrowed { name } => write!(f, "VirtualMonitor::Borrowed({name})"),
        }
    }
}

impl VirtualMonitor {
    /// Reuse or plug a Parsec virtual monitor at `width`×`height`@`hz`. The keepalive runs in this process; if the
    /// app dies, the driver drops the monitor about a second later, and the guard process removes it at once.
    ///
    /// # Errors
    /// When the driver is missing, or the monitor does not attach or take the mode (it is unplugged again then).
    pub(crate) fn acquire(width: u32, height: u32, hz: u32, reuse: Reuse) -> Result<Self> {
        let mode = tunnelrez_vdd::Mode { width, height, hz }.capped();
        if reuse == Reuse::Existing {
            if let Some(name) = tunnelrez_vdd::vdd_display_names().into_iter().next() {
                // A reused monitor may show another size (e.g. 1080p while 4K was picked): switch it, never
                // stream at another size without saying so.
                tunnelrez_vdd::ensure_mode(&name, mode)
                    .map_err(|e| anyhow!("could not switch the TV monitor to {mode} ({name}): {e}"))?;
                return Ok(Self::Borrowed { name });
            }
        }
        let display = tunnelrez_vdd::VirtualDisplay::plug(mode).map_err(|e| anyhow!("{e}"))?;
        let name = display.device_name.clone().ok_or_else(|| anyhow!("the virtual monitor did not attach"))?;
        let guard = spawn_guard(display.index());
        Ok(Self::Owned { _display: display, name, guard })
    }

    /// GDI name, e.g. `\\.\DISPLAY11`.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Owned { name, .. } | Self::Borrowed { name } => name,
        }
    }
}

/// GDI names of the outputs that show a Parsec virtual display, attached ones first.
pub(crate) fn virtual_display_names() -> Vec<String> {
    tunnelrez_vdd::vdd_display_names()
}

/// Unplug every virtual display. For one left behind that will not take the mode: only a fresh plug helps then. Needs
/// no administrator rights.
///
/// # Errors
/// When the driver cannot be opened.
pub(crate) fn unplug_virtual_displays() -> Result<()> {
    tunnelrez_vdd::unplug_all().map_err(|e| anyhow!("{e}"))
}

// ------------------------------------------------------------------------------------------------ webview memory

/// How much memory the hidden panel's `WebView2` should keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebviewMemory {
    /// The panel is hidden: keep as little as possible.
    Low,
    /// The panel is shown.
    Normal,
}

/// Ask `WebView2` to keep as little memory as it can while the panel is hidden ([`WebviewMemory::Low`]), and to go
/// back to normal when it is shown. A hint only: failures (an old `WebView2` runtime without the API) are ignored.
pub(crate) fn webview_memory(window: &tauri::WebviewWindow, target: WebviewMemory) {
    use ::windows::core::Interface;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    let _ = window.with_webview(move |wv| {
        let level = match target {
            WebviewMemory::Low => COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
            WebviewMemory::Normal => COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        };
        // SAFETY: runs on the webview's own (main) thread, as WebView2 requires; the controller is alive for the call.
        let core = unsafe { wv.controller().CoreWebView2() };
        if let Ok(v19) = core.and_then(|c| c.cast::<ICoreWebView2_19>()) {
            // SAFETY: same thread and object as above; the level is a valid enum value.
            let _ = unsafe { v19.SetMemoryUsageTargetLevel(level) };
        }
    });
}

// ------------------------------------------------------------------------------------------------ session end

static ON_SESSION_END: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

unsafe extern "system" fn session_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION if wp.0 != 0 => {
            if let Some(f) = ON_SESSION_END.get() {
                f();
            }
            LRESULT(0)
        }
        // SAFETY: forwarding the unmodified message to the default window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Run `f` (synchronously, on a private thread) when Windows logs off or shuts down, so the stream stops with a
/// BYE and the virtual monitor is unplugged before the process is torn down. Call once.
///
/// # Errors
/// When the watcher thread cannot be started.
pub(crate) fn on_session_end(f: impl Fn() + Send + Sync + 'static) -> Result<()> {
    ON_SESSION_END.set(Box::new(f)).map_err(|_| anyhow!("on_session_end called twice"))?;
    std::thread::Builder::new()
        .name("session-end".into())
        .spawn(|| {
            // SAFETY: null module name = this executable; always valid.
            let Ok(hinst) = (unsafe { GetModuleHandleW(PCWSTR::null()) }) else { return };
            let class = w!("TunnelRezSessionWatcher");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(session_wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // SAFETY: `wc` is fully initialised and its class name is static.
            if unsafe { RegisterClassW(&raw const wc) } == 0 {
                return;
            }
            // A hidden top-level window (not message-only): only top-level windows get WM_QUERYENDSESSION.
            // SAFETY: the class was registered above; no parent, menu or create params.
            let created = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    w!(""),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(hinst.into()),
                    None,
                )
            };
            if created.is_err() {
                return;
            }
            let mut msg = MSG::default();
            // SAFETY: standard message loop on the thread that owns the window.
            while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
                // SAFETY: `msg` was filled by GetMessageW.
                let _ = unsafe { TranslateMessage(&raw const msg) };
                // SAFETY: `msg` was filled by GetMessageW.
                unsafe { DispatchMessageW(&raw const msg) };
            }
        })
        .context("start session-end watcher")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_target_is_the_exe() {
        assert_eq!(run_target(r#""C:\a b\tunnelrez.exe" --autostart"#), r"C:\a b\tunnelrez.exe");
        assert_eq!(run_target(r"C:\x\tunnelrez.exe --autostart"), r"C:\x\tunnelrez.exe");
    }
}
