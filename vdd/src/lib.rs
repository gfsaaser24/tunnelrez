//! Parsec Virtual Display Driver (VDD) control, after nomi-san/parsec-vdd `core/parsec-vdd.h`.
//!
//! The driver only keeps displays plugged while it is pinged (`IOCTL_UPDATE`) at least every ~100 ms; when the
//! pinging process exits or hangs, every display it added is unplugged about a second later. `VirtualDisplay`
//! owns that keepalive thread, so a crash of the sender can never leave a stray monitor behind.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{GUID, PCSTR, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsA, SetupDiGetDeviceInterfaceDetailA,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_A,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY,
    DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, DISP_CHANGE,
    DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileA, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_FLAG_WRITE_THROUGH,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::CreateEventA;
use windows::Win32::System::IO::{DeviceIoControl, GetOverlappedResultEx, OVERLAPPED};

/// {00b41627-04c4-429e-a26e-0265cf50c8fa}
const VDD_ADAPTER_GUID: GUID = GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);
/// The monitor id the driver reports (`HKLM\SYSTEM\CurrentControlSet\Enum\DISPLAY\PSCCDD0`).
pub const VDD_DISPLAY_ID: &str = "PSCCDD0";

const IOCTL_ADD: u32 = 0x0022_e004;
const IOCTL_REMOVE: u32 = 0x0022_a008;
const IOCTL_UPDATE: u32 = 0x0022_a00c;
const IOCTL_VERSION: u32 = 0x0022_e010;

/// How often [`Adapter::add_display`] asks, and how long it waits in between.
const ADD_TRIES: u32 = 10;
const ADD_RETRY: Duration = Duration::from_millis(500);

/// Display slots the driver has.
const MAX_DISPLAYS: u16 = 8;

/// Ping well under the driver's ~100 ms deadline.
const KEEPALIVE: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct VddError(pub String);

impl std::fmt::Display for VddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VddError {}

fn err<T>(msg: impl Into<String>) -> Result<T, VddError> {
    Err(VddError(msg.into()))
}

/// An open handle to the VDD adapter.
#[derive(Debug)]
struct Adapter(HANDLE);

// SAFETY: `Adapter` exclusively owns its device handle. The handle is only used for `DeviceIoControl` and
// `CloseHandle`, which are valid from any thread, and shared use is serialised by the `Mutex` it lives in.
unsafe impl Send for Adapter {}

impl Adapter {
    fn open() -> Result<Self, VddError> {
        // SAFETY: the GUID reference is valid for the call; the enumerator and flags are plain values and the
        // other arguments are null (no filter, no parent window).
        let info = unsafe {
            SetupDiGetClassDevsA(Some(&VDD_ADAPTER_GUID), PCSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
        }
        .map_err(|e| VddError(format!("Parsec VDD not found (is the driver installed?): {e}")))?;
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).expect("struct size fits in u32"),
            ..Default::default()
        };
        let mut i = 0;
        // SAFETY: `info` is the live device info set made above; the GUID and `iface` (with `cbSize` set) are
        // valid for the call.
        while unsafe {
            SetupDiEnumDeviceInterfaces(info, None, std::ptr::from_ref(&VDD_ADAPTER_GUID), i, &raw mut iface)
        }
        .is_ok()
        {
            i += 1;
            let mut size = 0u32;
            // SAFETY: first call of the two-call pattern: a null detail buffer of size 0 only asks for the
            // required size, which is written to `size`; `iface` was filled by the enumeration above.
            let _ =
                unsafe { SetupDiGetDeviceInterfaceDetailA(info, &raw const iface, None, 0, Some(&raw mut size), None) };
            if size == 0 {
                continue;
            }
            // The detail struct is variable-length; back it with u64s for alignment.
            let mut buf = vec![0u64; usize::try_from(size).expect("u32 fits in usize").div_ceil(8)];
            let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_A>();
            let detail_size =
                u32::try_from(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_A>()).expect("struct size fits in u32");
            // SAFETY: `detail` points to the start of `buf`, which is zeroed, 8-byte aligned and at least `size`
            // bytes, and `size` covers the fixed header that holds `cbSize`.
            unsafe { (*detail).cbSize = detail_size };
            // SAFETY: `detail` points to a buffer of `size` bytes whose `cbSize` is set; `iface` is valid.
            let got = unsafe {
                SetupDiGetDeviceInterfaceDetailA(info, &raw const iface, Some(detail), size, Some(&raw mut size), None)
            };
            if got.is_err() {
                continue;
            }
            // SAFETY: the call above filled `buf`; `DevicePath` is its NUL-terminated string, and `buf` outlives
            // the pointer's use below. Taking the address with `&raw` avoids a reference to a packed field.
            let path = PCSTR(unsafe { &raw const (*detail).DevicePath }.cast::<u8>());
            // SAFETY: `path` is a NUL-terminated string inside `buf`, which is alive; the other arguments are
            // plain flags or null (default security, no template file).
            let handle = unsafe {
                CreateFileA(
                    path,
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED | FILE_FLAG_WRITE_THROUGH,
                    None,
                )
            };
            if let Ok(h) = handle {
                if !h.is_invalid() {
                    destroy_info_list(info);
                    return Ok(Adapter(h));
                }
            }
        }
        destroy_info_list(info);
        err("could not open the Parsec Virtual Display Adapter")
    }

    /// One IOCTL with a 32-byte input buffer and a u32 result, as the driver expects. Returns None on failure.
    fn ioctl(&self, code: u32, data: &[u8]) -> Option<u32> {
        let mut input = [0u8; 32];
        input[..data.len().min(32)].copy_from_slice(&data[..data.len().min(32)]);
        let input_len = u32::try_from(input.len()).expect("32 fits in u32");
        let mut output = 0u32;
        // SAFETY: all arguments are null or plain values (unnamed, manual-reset, initially unsignalled event).
        let event = unsafe { CreateEventA(None, true, false, PCSTR::null()) }.ok()?;
        let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
        // SAFETY: `self.0` is an open overlapped device handle; `input` and `output` outlive the request because
        // we wait for it below before returning, and their lengths (32 and 4 bytes) match what is passed.
        let _ = unsafe {
            DeviceIoControl(
                self.0,
                code,
                Some(input.as_ptr().cast::<c_void>()),
                input_len,
                Some((&raw mut output).cast::<c_void>()),
                4,
                None,
                Some(&raw mut ov),
            )
        };
        let mut transferred = 0u32;
        // SAFETY: `ov` is the OVERLAPPED used for the request above and `transferred` is a valid u32.
        let ok = unsafe { GetOverlappedResultEx(self.0, &raw const ov, &raw mut transferred, 5000, false) }.is_ok();
        // SAFETY: `event` was created above and is closed exactly once, after the wait ended.
        let _ = unsafe { CloseHandle(event) };
        ok.then_some(output)
    }

    fn update(&self) {
        let _ = self.ioctl(IOCTL_UPDATE, &[]);
    }

    /// Add a display. For about three seconds after a display was removed the driver refuses at once, so this
    /// tries for a little longer than that.
    fn add_display(&self) -> Option<u32> {
        for _ in 1..ADD_TRIES {
            if let Some(index) = self.ioctl(IOCTL_ADD, &[]) {
                return Some(index);
            }
            self.update();
            std::thread::sleep(ADD_RETRY);
        }
        self.ioctl(IOCTL_ADD, &[])
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the handle opened in `open` and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Free a device info set from `SetupDiGetClassDevsA`.
fn destroy_info_list(info: HDEVINFO) {
    // SAFETY: `info` came from `SetupDiGetClassDevsA` and is destroyed once, by the caller that owns it.
    let _ = unsafe { SetupDiDestroyDeviceInfoList(info) };
}

/// Display mode for the virtual monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

impl std::str::FromStr for Mode {
    type Err = VddError;
    /// `1920x1080@60`
    fn from_str(s: &str) -> Result<Self, VddError> {
        let (size, hz) = s.split_once('@').unwrap_or((s, "60"));
        let (w, h) =
            size.split_once(['x', 'X']).ok_or_else(|| VddError(format!("bad mode {s:?}, want 1920x1080@60")))?;
        let p = |v: &str| v.trim().parse::<u32>().map_err(|_| VddError(format!("bad mode {s:?}")));
        Ok(Mode { width: p(w)?, height: p(h)?, hz: p(hz)? })
    }
}

/// One plugged virtual monitor. Dropping it unplugs the monitor and stops the keepalive.
#[derive(Debug)]
pub struct VirtualDisplay {
    adapter: Arc<Mutex<Adapter>>,
    index: u32,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    /// GDI device name, e.g. `\\.\DISPLAY5`, once Windows has attached it.
    pub device_name: Option<String>,
    pub driver_version: Option<u32>,
}

impl VirtualDisplay {
    /// Plug one virtual monitor, start the keepalive, wait for Windows to attach it, and make sure it runs `mode`
    /// (see [`ensure_mode`]; refresh capped at 60 Hz).
    ///
    /// # Errors
    /// When the driver is missing or refuses the request, the keepalive thread cannot start, or the monitor
    /// does not show `mode` in time.
    pub fn plug(mode: Mode) -> Result<Self, VddError> {
        let adapter = Adapter::open()?;
        let driver_version = adapter.ioctl(IOCTL_VERSION, &[]);
        let before = attached_names();
        let index = adapter.add_display().ok_or_else(|| VddError("IOCTL_ADD failed".into()))?;
        adapter.update();

        let adapter = Arc::new(Mutex::new(adapter));
        let stop = Arc::new(AtomicBool::new(false));
        let pinger = {
            let (adapter, stop) = (Arc::clone(&adapter), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("vdd-keepalive".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(a) = adapter.lock() {
                            a.update();
                        }
                        std::thread::sleep(KEEPALIVE);
                    }
                })
                .map_err(|e| VddError(format!("keepalive thread: {e}")))?
        };

        let mut vd = VirtualDisplay { adapter, index, stop, pinger: Some(pinger), device_name: None, driver_version };

        // LRT_VDD_NOMODE=1: plug only, leave the mode to Windows (for debugging).
        let set_mode = std::env::var_os("LRT_VDD_NOMODE").is_none();
        let name = wait_for_name(&before);
        vd.device_name.clone_from(&name);
        if let Some(name) = name.filter(|_| set_mode) {
            if let Err(e) = ensure_mode(&name, mode) {
                // Windows attached it under another name meanwhile (it can take a while on a new set of screens).
                let Some(other) = pick_name(&before, &vdd_displays(), Patience::Waiting).filter(|o| *o != name) else {
                    return Err(e);
                };
                ensure_mode(&other, mode)?;
                vd.device_name = Some(other);
            }
        }
        Ok(vd)
    }

    pub fn index(&self) -> u32 {
        self.index
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(p) = self.pinger.take() {
            let _ = p.join();
        }
        if let Ok(a) = self.adapter.lock() {
            // The driver wants the index as a 16-bit big-endian value.
            let [_, _, hi, lo] = self.index.to_be_bytes();
            let idx = [hi, lo];
            let _ = a.ioctl(IOCTL_REMOVE, &idx);
            a.update();
        }
    }
}

/// Unplug the virtual display with driver index `index` (from [`VirtualDisplay::index`]) without pinging.
///
/// For a guard process that cleans up after a crashed owner: while any other process keeps pinging the driver
/// (another `tunnelrez-vdd run`, say), the driver does not drop an orphaned display by itself.
///
/// # Errors
/// When the driver cannot be opened, `index` does not fit in 16 bits, or the driver rejects the removal.
pub fn unplug_index(index: u32) -> Result<(), VddError> {
    let adapter = Adapter::open()?;
    let idx = u16::try_from(index).map_err(|_| VddError(format!("bad display index {index}")))?.to_be_bytes();
    adapter.ioctl(IOCTL_REMOVE, &idx).ok_or_else(|| VddError("IOCTL_REMOVE failed".into()))?;
    adapter.update();
    Ok(())
}

/// Unplug every virtual display, whoever plugged it. For a display left behind by a process that still pings the
/// driver: nothing else removes it. Needs no administrator rights.
///
/// # Errors
/// When the driver cannot be opened.
pub fn unplug_all() -> Result<(), VddError> {
    let adapter = Adapter::open()?;
    for index in 0..MAX_DISPLAYS {
        if vdd_displays().is_empty() {
            break;
        }
        // The driver ignores an index with no display on it, but takes a second over it: stop when none are left.
        let _ = adapter.ioctl(IOCTL_REMOVE, &index.to_be_bytes());
        adapter.update();
    }
    Ok(())
}

/// Whether a detached output may stand for the new monitor yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Patience {
    /// Windows may still attach the monitor by itself.
    Waiting,
    /// It did not: the monitor stays detached until it is attached here.
    Over,
}

/// The GDI name of a monitor just plugged. Windows restores the saved layout by itself, under the name it decides:
/// an attached display that was not attached `before` is the new one (one attached before is someone else's). With no
/// saved layout the monitor stays detached and shows up on every idle output of the adapter; a name in that list
/// means nothing until Windows has had its chance, and any of them can then be attached.
fn pick_name(before: &[String], now: &[VddDisplay], patience: Patience) -> Option<String> {
    let new = now.iter().find(|d| d.attached && !before.contains(&d.name));
    let idle = now.iter().find(|d| !d.attached).filter(|_| patience == Patience::Over);
    new.or(idle).map(|d| d.name.clone())
}

/// [`pick_name`] until a name comes (at most ~5 s).
fn wait_for_name(before: &[String]) -> Option<String> {
    let start = std::time::Instant::now();
    loop {
        let patience = if start.elapsed() < ATTACH_GRACE { Patience::Waiting } else { Patience::Over };
        if let Some(name) = pick_name(before, &vdd_displays(), patience) {
            return Some(name);
        }
        if start.elapsed() >= MODE_TIMEOUT {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// GDI names of the virtual displays on the desktop now.
fn attached_names() -> Vec<String> {
    vdd_displays().into_iter().filter(|d| d.attached).map(|d| d.name).collect()
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

fn display_device(index: u32, parent: Option<&[u16]>) -> Option<DISPLAY_DEVICEW> {
    let mut dd = DISPLAY_DEVICEW {
        cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).expect("struct size fits in u32"),
        ..Default::default()
    };
    let device = parent.map_or_else(PCWSTR::null, |p| PCWSTR(p.as_ptr()));
    // SAFETY: `device` is null or a NUL-terminated UTF-16 string borrowed from `parent`; `dd` is a DISPLAY_DEVICEW
    // we own with `cb` set.
    let ok = unsafe { EnumDisplayDevicesW(device, index, &raw mut dd, 0) };
    ok.as_bool().then_some(dd)
}

/// GDI names (`\\.\DISPLAYn`) of every output whose monitor is a Parsec virtual display.
pub fn vdd_display_names() -> Vec<String> {
    vdd_displays().into_iter().map(|d| d.name).collect()
}

/// One plugged Parsec virtual display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VddDisplay {
    /// GDI name, `\\.\DISPLAYn`.
    pub name: String,
    /// Part of the desktop. A detached display shows nothing and cannot be captured.
    pub attached: bool,
}

/// The outputs that show a Parsec virtual display, attached ones first. A detached monitor shows up on every idle
/// output of the adapter (16 names), so this counts outputs, not monitors.
pub fn vdd_displays() -> Vec<VddDisplay> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(adapter) = display_device(i, None) {
        i += 1;
        let mut j = 0;
        while let Some(mon) = display_device(j, Some(&adapter.DeviceName)) {
            j += 1;
            let id = wide_to_string(&mon.DeviceID);
            if id.contains(VDD_DISPLAY_ID) {
                let attached = adapter.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0;
                out.push(VddDisplay { name: wide_to_string(&adapter.DeviceName), attached });
                break;
            }
        }
    }
    out.sort_by_key(|d| !d.attached);
    out
}

/// A zeroed `DEVMODEW` with `dmSize` set, as the display APIs require.
fn new_devmode() -> DEVMODEW {
    DEVMODEW { dmSize: u16::try_from(size_of::<DEVMODEW>()).expect("struct size fits in u16"), ..Default::default() }
}

fn current_mode(name: &[u16]) -> Option<DEVMODEW> {
    let mut dm = new_devmode();
    // SAFETY: `name` is a NUL-terminated UTF-16 string and `dm` is a sized DEVMODEW we own.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &raw mut dm) };
    ok.as_bool().then_some(dm)
}

/// The modes Windows offers for `name`.
fn offered_modes(name: &[u16]) -> Vec<Mode> {
    let mut out: Vec<Mode> = Vec::new();
    for i in 0..MAX_MODES {
        let mut dm = new_devmode();
        // SAFETY: `name` is a NUL-terminated UTF-16 string and `dm` is a sized DEVMODEW we own.
        let ok = unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &raw mut dm) };
        if !ok.as_bool() {
            break;
        }
        let m = Mode { width: dm.dmPelsWidth, height: dm.dmPelsHeight, hz: dm.dmDisplayFrequency };
        if out.last() != Some(&m) {
            out.push(m);
        }
    }
    out
}

/// Highest refresh rate `TunnelRez` uses.
pub const MAX_HZ: u32 = 60;

/// How long to wait for the monitor to be attached with the requested mode.
const MODE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long Windows gets to attach a new monitor by itself (from a saved layout, within ~100 ms when there is one).
const ATTACH_GRACE: Duration = Duration::from_millis(1500);

/// How long one mode change gets to show before it is sent again: each try makes every screen blink.
const MODE_SETTLE: Duration = Duration::from_secs(1);

/// Most `EnumDisplaySettings` entries to read for one monitor (one per size, rate, depth and scaling).
const MAX_MODES: u32 = 4096;

impl Mode {
    /// The mode `TunnelRez` really asks for: refresh capped at [`MAX_HZ`].
    #[must_use]
    pub fn capped(self) -> Mode {
        Mode { hz: self.hz.clamp(1, MAX_HZ), ..self }
    }

    /// Same size, refresh within 1 Hz (Windows reports 59 for 59.94).
    pub fn matches(self, other: Mode) -> bool {
        self.width == other.width && self.height == other.height && self.hz.abs_diff(other.hz) <= 1
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.hz)
    }
}

/// What the mode loop does next.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// The monitor already shows the requested mode: do not touch it.
    Done,
    /// The monitor offers this mode (`want`, or the same within 1 Hz) and does not show it: set it. That also
    /// attaches a monitor that is detached from the desktop.
    Set(Mode),
    /// The requested mode is not offered (yet): wait.
    Wait,
}

/// `current` is `None` while the monitor is not attached to the desktop. The saved layout decides that: Windows
/// attaches a new monitor by itself only when it remembers this set of screens, so a detached monitor is normal and
/// is attached here. What it shows now does not matter, only that `want` is on offer.
fn next_step(current: Option<Mode>, offered: &[Mode], want: Mode) -> Step {
    if current.is_some_and(|c| c.matches(want)) {
        return Step::Done;
    }
    offered.iter().find(|m| m.matches(want)).map_or(Step::Wait, |m| Step::Set(*m))
}

/// Make sure the virtual monitor `name` runs `mode` (refresh capped at 60 Hz).
///
/// Waits until the monitor offers `mode` (a monitor rendered on another adapter, e.g. the NVIDIA GPU, gets its modes
/// after its name, and Windows often restores the saved layout by itself). If it already shows the mode, nothing is
/// changed. Otherwise the mode is set and checked, at the monitor's current position; a monitor with no position yet
/// (detached, or still overlapping the primary) goes right of the rightmost screen, where it overlaps nothing, so no
/// other screen moves. Only `name` is ever changed, never another display.
///
/// # Errors
/// When the monitor still does not show `mode` after ~5 s.
pub fn ensure_mode(name: &str, mode: Mode) -> Result<(), VddError> {
    let want = mode.capped();
    let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let start = std::time::Instant::now();
    let mut last_code = None;
    let mut last_try: Option<std::time::Instant> = None;
    loop {
        let cur = current_mode(&wname);
        let cur_mode = cur.map(|d| Mode { width: d.dmPelsWidth, height: d.dmPelsHeight, hz: d.dmDisplayFrequency });
        let offered = offered_modes(&wname);
        match next_step(cur_mode, &offered, want) {
            Step::Done => return Ok(()),
            Step::Set(target) if last_try.is_none_or(|t| t.elapsed() >= MODE_SETTLE) => {
                // SAFETY: reading the POINTL position out of a DEVMODEW filled by EnumDisplaySettingsW (display
                // devices use the position variant of the union).
                let pos = cur.map(|d| unsafe { d.Anonymous1.Anonymous2.dmPosition });
                let at = placement(pos.map(|p| (p.x, p.y)), role(&wname), || free_spot(&wname));
                let code = apply_mode(&wname, target, at);
                last_code = (code != DISP_CHANGE_SUCCESSFUL).then_some(code.0);
                last_try = Some(std::time::Instant::now());
            }
            Step::Set(_) | Step::Wait => {}
        }
        if start.elapsed() >= MODE_TIMEOUT {
            let on_offer =
                if offered.iter().any(|m| m.matches(want)) { "it is on offer" } else { "it is not on offer" };
            return err(format!(
                "could not set {want} on {name}: it shows {}{}; {} modes offered, {on_offer}",
                cur_mode.map_or_else(|| "nothing (not attached)".to_string(), |m| m.to_string()),
                last_code.map(|c| format!(" (last error code {c})")).unwrap_or_default(),
                offered.len()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Attach the virtual monitor `name` when it is detached (at `mode`); an attached one is left as it is, whatever it
/// shows. For a caller that takes the monitor's size as it comes (`--size native`).
///
/// # Errors
/// When a detached monitor does not attach with `mode` (see [`ensure_mode`]).
pub fn ensure_attached(name: &str, mode: Mode) -> Result<(), VddError> {
    if vdd_displays().iter().any(|d| d.name == name && d.attached) {
        return Ok(());
    }
    ensure_mode(name, mode)
}

/// Where to put `name` when its mode changes: its current desktop position, kept exactly (zero and negative
/// coordinates are real placements: above, below or left of the primary). Only (0, 0) on a display that is not
/// the primary means "not placed yet" (it would overlap the primary, which always sits at (0, 0)); then, as for a
/// display with no position at all, the free spot.
fn placement(current: Option<(i32, i32)>, role: Role, free_spot: impl FnOnce() -> (i32, i32)) -> (i32, i32) {
    match current {
        Some((0, 0)) if role == Role::Secondary => free_spot(),
        Some(p) => p,
        None => free_spot(),
    }
}

/// Whether a display is the primary one (always at desktop (0, 0)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Primary,
    Secondary,
}

/// The role of `wname` (NUL-terminated UTF-16 device name); [`Role::Secondary`] when it is not found.
fn role(wname: &[u16]) -> Role {
    let name = wname.strip_suffix(&[0]).unwrap_or(wname);
    let mut i = 0;
    while let Some(adapter) = display_device(i, None) {
        i += 1;
        let end = adapter.DeviceName.iter().position(|&c| c == 0).unwrap_or(adapter.DeviceName.len());
        if adapter.DeviceName[..end] == *name {
            return if adapter.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0 {
                Role::Primary
            } else {
                Role::Secondary
            };
        }
    }
    Role::Secondary
}

/// One attached screen's place on the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Screen {
    x: i32,
    y: i32,
    width: i32,
}

/// The top-left corner of a spot right of every screen, touching the rightmost one (Windows moves a screen that
/// touches nothing). Nothing overlaps it, so placing a monitor there moves no other screen.
fn beside_all(screens: &[Screen]) -> (i32, i32) {
    screens.iter().max_by_key(|s| s.x + s.width).map_or((1920, 0), |s| (s.x + s.width, s.y))
}

/// [`beside_all`] for the screens attached now, other than `wname` itself (NUL-terminated UTF-16).
fn free_spot(wname: &[u16]) -> (i32, i32) {
    let me = wname.strip_suffix(&[0]).unwrap_or(wname);
    let mut screens = Vec::new();
    let mut i = 0;
    while let Some(adapter) = display_device(i, None) {
        i += 1;
        let end = adapter.DeviceName.iter().position(|&c| c == 0).unwrap_or(adapter.DeviceName.len());
        if adapter.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0 || adapter.DeviceName[..end] == *me {
            continue;
        }
        if let Some(dm) = current_mode(&adapter.DeviceName) {
            // SAFETY: position variant of the DEVMODEW union, filled by EnumDisplaySettingsW.
            let at = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
            screens.push(Screen { x: at.x, y: at.y, width: dm.dmPelsWidth.cast_signed() });
        }
    }
    beside_all(&screens)
}

/// Set `mode` at desktop position `at` on `wname` only, then apply. Returns the first failing result code.
fn apply_mode(wname: &[u16], mode: Mode, at: (i32, i32)) -> DISP_CHANGE {
    let mut dm = new_devmode();
    dm.dmPelsWidth = mode.width;
    dm.dmPelsHeight = mode.height;
    dm.dmDisplayFrequency = mode.hz;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_POSITION;
    dm.Anonymous1.Anonymous2.dmPosition.x = at.0;
    dm.Anonymous1.Anonymous2.dmPosition.y = at.1;
    // SAFETY: `wname` is NUL-terminated UTF-16 and `dm` is a fully initialised DEVMODEW that outlives the call.
    let r = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(wname.as_ptr()),
            Some(&raw const dm),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        )
    };
    if r != DISP_CHANGE_SUCCESSFUL {
        return r;
    }
    // Apply all pending changes at once.
    // SAFETY: a null device name and null mode apply the changes already written to the registry.
    unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: Mode = Mode { width: 1920, height: 1080, hz: 60 };
    const UHD: Mode = Mode { width: 3840, height: 2160, hz: 60 };

    #[test]
    fn saved_mode_that_matches_is_left_alone() {
        assert_eq!(next_step(Some(HD), &[], HD), Step::Done);
        assert_eq!(next_step(Some(Mode { hz: 59, ..HD }), &[HD], HD), Step::Done);
    }

    #[test]
    fn a_detached_monitor_that_offers_the_mode_gets_it() {
        // The failure after a reboot: no current mode, the mode on offer. Waiting here never ended.
        assert_eq!(next_step(None, &[HD, UHD], UHD), Step::Set(UHD));
        assert_eq!(next_step(None, &[HD], HD), Step::Set(HD));
    }

    #[test]
    fn waits_until_the_mode_is_on_offer() {
        assert_eq!(next_step(None, &[], HD), Step::Wait);
        assert_eq!(next_step(Some(HD), &[], UHD), Step::Wait);
        assert_eq!(next_step(None, &[HD], UHD), Step::Wait, "the list is still filling in");
        assert_eq!(next_step(Some(HD), &[HD], UHD), Step::Wait);
        assert_eq!(next_step(Some(HD), &[HD, UHD], UHD), Step::Set(UHD));
    }

    #[test]
    fn the_current_mode_need_not_be_on_offer() {
        let saved = Mode { width: 2560, height: 1440, hz: 60 };
        assert_eq!(next_step(Some(saved), &[HD, UHD], UHD), Step::Set(UHD));
    }

    #[test]
    fn the_offered_rate_is_the_one_set() {
        // Windows lists 59 Hz for 59.94: asking for exactly 60 would be refused.
        let offered = Mode { hz: 59, ..UHD };
        assert_eq!(next_step(None, &[HD, offered], UHD), Step::Set(offered));
    }

    fn attached(name: &str) -> VddDisplay {
        VddDisplay { name: name.into(), attached: true }
    }

    fn idle(name: &str) -> VddDisplay {
        VddDisplay { name: name.into(), attached: false }
    }

    #[test]
    fn the_new_monitor_is_the_one_windows_attached() {
        let now = [attached("A"), idle("B")];
        assert_eq!(pick_name(&[], &now, Patience::Waiting).as_deref(), Some("A"));
        assert_eq!(pick_name(&["A".into()], &now, Patience::Waiting), None, "A is someone else's monitor");
    }

    #[test]
    fn an_idle_output_counts_only_after_windows_had_its_chance() {
        // The race that picked a name Windows did not use: every idle output lists the monitor for a moment.
        let outputs = [idle("X"), idle("Y")];
        assert_eq!(pick_name(&[], &outputs, Patience::Waiting), None);
        assert_eq!(pick_name(&[], &outputs, Patience::Over).as_deref(), Some("X"));
        assert_eq!(pick_name(&[], &[], Patience::Over), None);
    }

    #[test]
    fn refresh_capped_at_60() {
        let m: Mode = "3840x2160@120".parse().unwrap();
        assert_eq!(m.capped(), UHD);
        assert_eq!("1920x1080".parse::<Mode>().unwrap().capped(), HD);
        assert_eq!(UHD.to_string(), "3840x2160@60");
    }

    #[test]
    fn mode_change_keeps_placement() {
        let free = || (2560, 0);
        // Above the primary, and beside it with a vertical offset: kept exactly.
        assert_eq!(placement(Some((0, -1080)), Role::Secondary, free), (0, -1080));
        assert_eq!(placement(Some((2560, 400)), Role::Secondary, free), (2560, 400));
        assert_eq!(placement(Some((-1920, 0)), Role::Secondary, free), (-1920, 0));
        // Not placed yet (overlaps the primary) or no position: the free spot.
        assert_eq!(placement(Some((0, 0)), Role::Secondary, free), (2560, 0));
        assert_eq!(placement(None, Role::Secondary, free), (2560, 0));
        // The primary itself stays at the origin.
        assert_eq!(placement(Some((0, 0)), Role::Primary, free), (0, 0));
    }

    #[test]
    fn the_free_spot_overlaps_no_screen() {
        let laptop = Screen { x: 0, y: 0, width: 2560 };
        let above = Screen { x: 712, y: -600, width: 1024 };
        assert_eq!(beside_all(&[laptop, above]), (2560, 0), "a small screen above the panel is not in the way");
        let right = Screen { x: 2560, y: 200, width: 1920 };
        assert_eq!(beside_all(&[laptop, right]), (4480, 200), "right of the screen right of the primary");
        let left = Screen { x: -1920, y: 0, width: 1920 };
        assert_eq!(beside_all(&[left, laptop]), (2560, 0));
        assert_eq!(beside_all(&[]), (1920, 0), "nothing known");
    }
}
