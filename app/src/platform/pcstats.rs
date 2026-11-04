//! Readings of this PC for the stream logger: CPU use, NVIDIA GPU load, and the Wi-Fi link.
//!
//! This module never reads or returns an SSID, a BSSID or an IP address. The BSSID of the current link is held
//! in memory for one call only, to find the matching BSS entry, and is then dropped.

use std::ffi::{c_void, CStr};
use std::mem::{offset_of, size_of};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr::{null_mut, NonNull};

use ::windows::core::{s, w, GUID, PCSTR};
use ::windows::Win32::Foundation::{
    FreeLibrary, ERROR_ACCESS_DENIED, ERROR_SERVICE_DISABLED, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
    FILETIME, HANDLE, HMODULE, WIN32_ERROR,
};
use ::windows::Win32::NetworkManagement::WiFi::{
    dot11_BSS_type_infrastructure, wlan_interface_state_connected, wlan_intf_opcode_channel_number,
    wlan_intf_opcode_current_connection, wlan_intf_opcode_rssi, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory,
    WlanGetNetworkBssList, WlanOpenHandle, WlanQueryInterface, WLAN_ASSOCIATION_ATTRIBUTES, WLAN_BSS_ENTRY,
    WLAN_BSS_LIST, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO, WLAN_INTERFACE_INFO_LIST, WLAN_INTF_OPCODE,
};
use ::windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use ::windows::Win32::System::SystemInformation::GetLocalTime;
use ::windows::Win32::System::Threading::GetSystemTimes;

// ------------------------------------------------------------------------------------------------ public types

/// One reading of this PC for the logger. Anything that cannot be read is None.
#[derive(serde::Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct PcReading {
    /// Whole-machine CPU use since the previous reading, percent (None on the first reading).
    pub(crate) cpu_pct: Option<f64>,
    pub(crate) gpu: Option<GpuReading>,
    pub(crate) wifi: Wifi,
}

/// GPU 0 (NVIDIA only).
#[derive(serde::Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct GpuReading {
    /// For example `NVIDIA GeForce RTX 4070 Laptop GPU`.
    pub(crate) name: String,
    /// "nvml" or "nvidia-smi".
    pub(crate) source: &'static str,
    pub(crate) core_pct: Option<f64>,
    pub(crate) encoder_pct: Option<f64>,
    pub(crate) decoder_pct: Option<f64>,
    /// Graphics clock.
    pub(crate) clock_mhz: Option<f64>,
    pub(crate) power_w: Option<f64>,
    pub(crate) temp_c: Option<f64>,
}

/// State of the Wi-Fi link.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Wifi {
    Connected(WifiLink),
    /// A Wi-Fi adapter exists but is not connected (or the PC is on Ethernet only).
    NotConnected,
    /// No WLAN service/adapter, or Windows refused (e.g. 24H2 location privacy: `ERROR_ACCESS_DENIED`).
    /// `reason` is plain words.
    Unavailable {
        reason: String,
    },
}

impl Default for Wifi {
    fn default() -> Self {
        Wifi::Unavailable { reason: "Not read yet".to_owned() }
    }
}

/// The current Wi-Fi link. No SSID, BSSID or IP address, on purpose.
#[derive(serde::Serialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct WifiLink {
    /// "2.4 GHz", "5 GHz" or "6 GHz".
    pub(crate) band: Option<String>,
    pub(crate) channel: Option<u32>,
    pub(crate) frequency_mhz: Option<u32>,
    /// For example "802.11ax (Wi-Fi 6/6E)".
    pub(crate) phy: Option<String>,
    /// Receive link rate, Mb/s.
    pub(crate) rx_mbps: Option<f64>,
    /// Transmit link rate, Mb/s.
    pub(crate) tx_mbps: Option<f64>,
    /// Signal quality 0-100 (Windows `wlanSignalQuality`).
    pub(crate) signal_pct: Option<u32>,
    pub(crate) rssi_dbm: Option<i32>,
}

/// Reads the PC once per call. Create it on the thread that uses it (it holds raw handles; it is not Send).
pub(crate) struct PcProbe {
    cpu_prev: Option<CpuTimes>,
    gpu: Gpu,
    /// Opened once and reused. None when the WLAN service could not be opened; `read` tries again.
    wlan: Result<WlanClient, WIN32_ERROR>,
}

impl std::fmt::Debug for PcProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gpu = match &self.gpu {
            Gpu::Nvml(n) => format!("nvml ({})", n.name),
            Gpu::Smi => "nvidia-smi".to_owned(),
            Gpu::Absent => "none".to_owned(),
        };
        f.debug_struct("PcProbe")
            .field("cpu_primed", &self.cpu_prev.is_some())
            .field("gpu", &gpu)
            .field("wlan_open", &self.wlan.is_ok())
            .finish()
    }
}

impl PcProbe {
    /// Never fails: sources that cannot open are just absent.
    pub(crate) fn new() -> Self {
        let gpu = match Nvml::open() {
            Some(n) => Gpu::Nvml(n),
            None if smi_read().is_some() => Gpu::Smi,
            None => Gpu::Absent,
        };
        PcProbe { cpu_prev: cpu_times(), gpu, wlan: WlanClient::open() }
    }

    /// One reading. Cheap enough to call once per second (NVML and WLAN calls only; nvidia-smi only as fallback).
    pub(crate) fn read(&mut self) -> PcReading {
        let now = cpu_times();
        let cpu_pct = match (self.cpu_prev, now) {
            (Some(prev), Some(now)) => cpu_percent(prev, now),
            _ => None,
        };
        if now.is_some() {
            self.cpu_prev = now;
        }

        let gpu = match &self.gpu {
            Gpu::Nvml(n) => Some(n.read()),
            Gpu::Smi => smi_read(),
            Gpu::Absent => None,
        };

        if self.wlan.is_err() {
            self.wlan = WlanClient::open();
        }
        let wifi = match &self.wlan {
            Ok(client) => client.read(),
            Err(e) => Wifi::Unavailable { reason: wlan_reason(*e) },
        };

        PcReading { cpu_pct, gpu, wifi }
    }
}

/// Local wall-clock time now (for file names and log headers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalTime {
    pub(crate) year: u16,
    pub(crate) month: u16,
    pub(crate) day: u16,
    pub(crate) hour: u16,
    pub(crate) minute: u16,
    pub(crate) second: u16,
}

/// Local wall-clock time now.
pub(crate) fn local_time() -> LocalTime {
    // SAFETY: GetLocalTime has no preconditions; it only returns a SYSTEMTIME by value.
    let t = unsafe { GetLocalTime() };
    LocalTime { year: t.wYear, month: t.wMonth, day: t.wDay, hour: t.wHour, minute: t.wMinute, second: t.wSecond }
}

// ------------------------------------------------------------------------------------------------ CPU

/// System times in 100 ns ticks. Kernel time includes idle time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

fn filetime_ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

fn cpu_times() -> Option<CpuTimes> {
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: three valid, writable out-pointers to local FILETIMEs; the call only writes them.
    unsafe { GetSystemTimes(Some(&raw mut idle), Some(&raw mut kernel), Some(&raw mut user)) }.ok()?;
    Some(CpuTimes { idle: filetime_ticks(idle), kernel: filetime_ticks(kernel), user: filetime_ticks(user) })
}

/// CPU use between two samples, percent: `100 * (1 - d_idle / (d_kernel + d_user))`.
/// None when no time passed or the counters went backwards.
fn cpu_percent(prev: CpuTimes, now: CpuTimes) -> Option<f64> {
    let idle = now.idle.checked_sub(prev.idle)?;
    let total = now.kernel.checked_sub(prev.kernel)?.checked_add(now.user.checked_sub(prev.user)?)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    // Hundredths of a percent, computed in integers so no lossy float cast is needed. busy <= total, so <= 10_000.
    let hundredths = u128::from(busy) * 10_000 / u128::from(total);
    Some(f64::from(u32::try_from(hundredths).ok()?) / 100.0)
}

// ------------------------------------------------------------------------------------------------ GPU

enum Gpu {
    Nvml(Nvml),
    Smi,
    Absent,
}

/// `CREATE_NO_WINDOW`: nvidia-smi gets a console, but no window ever appears.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// GPU 0 from `nvidia-smi`. About 100-300 ms; only used when NVML is not available.
fn smi_read() -> Option<GpuReading> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,utilization.encoder,utilization.decoder,clocks.gr,power.draw,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_smi_line(String::from_utf8_lossy(&out.stdout).lines().next()?)
}

/// Parse one `nvidia-smi --format=csv,noheader,nounits` line. Fields that are not numbers ("[N/A]") are None.
fn parse_smi_line(line: &str) -> Option<GpuReading> {
    // Split from the right: the six numbers are last, so a comma inside the name cannot shift them.
    let fields: Vec<&str> = line.trim().rsplitn(7, ',').map(str::trim).collect();
    let [temp, power, clock, decoder, encoder, core, name] = fields.as_slice() else {
        return None;
    };
    if name.is_empty() {
        return None;
    }
    let num = |s: &str| s.parse::<f64>().ok().filter(|v| v.is_finite());
    Some(GpuReading {
        name: (*name).to_owned(),
        source: "nvidia-smi",
        core_pct: num(core),
        encoder_pct: num(encoder),
        decoder_pct: num(decoder),
        clock_mhz: num(clock),
        power_w: num(power),
        temp_c: num(temp),
    })
}

/// `nvmlReturn_t`; 0 is `NVML_SUCCESS`.
type NvmlReturn = i32;
const NVML_SUCCESS: NvmlReturn = 0;
/// `nvmlDevice_t`, an opaque handle.
type NvmlDevice = *mut c_void;
/// `NVML_CLOCK_GRAPHICS`.
const NVML_CLOCK_GRAPHICS: i32 = 0;
/// `NVML_TEMPERATURE_GPU`.
const NVML_TEMPERATURE_GPU: i32 = 0;
/// `NVML_DEVICE_NAME_V2_BUFFER_SIZE`.
const NVML_NAME_LEN: usize = 96;

/// `nvmlUtilization_t`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

type NoArgFn = unsafe extern "C" fn() -> NvmlReturn;
type HandleByIndexFn = unsafe extern "C" fn(u32, *mut NvmlDevice) -> NvmlReturn;
type NameFn = unsafe extern "C" fn(NvmlDevice, *mut u8, u32) -> NvmlReturn;
type UtilizationFn = unsafe extern "C" fn(NvmlDevice, *mut NvmlUtilization) -> NvmlReturn;
/// Encoder and decoder utilization: (device, percent, sampling period in us).
type CodecFn = unsafe extern "C" fn(NvmlDevice, *mut u32, *mut u32) -> NvmlReturn;
/// Clock and temperature: (device, enum selector, value).
type SelectFn = unsafe extern "C" fn(NvmlDevice, i32, *mut u32) -> NvmlReturn;
type PowerFn = unsafe extern "C" fn(NvmlDevice, *mut u32) -> NvmlReturn;

/// A DLL loaded with `LoadLibraryExW`, freed on drop.
struct Library(HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module came from LoadLibraryExW and is freed only here. Every owner of function pointers
        // into it (Nvml, NvmlSession) drops before this field does.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

/// Look up `name` in `lib` as a function pointer of type `F`.
///
/// # Safety
/// `F` must be the `unsafe extern "C" fn` type that matches the C declaration of `name`, and the pointer must
/// not be called after `lib` is freed.
unsafe fn symbol<F: Copy>(lib: &Library, name: PCSTR) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<unsafe extern "system" fn() -> isize>()) };
    // SAFETY: `lib.0` is a loaded module and `name` is a static NUL-terminated ASCII string.
    let f = unsafe { GetProcAddress(lib.0, name) }?;
    // SAFETY: both are plain function pointers of the same size (checked above); the caller guarantees `F` is
    // the real signature of the exported function.
    Some(unsafe { std::mem::transmute_copy::<unsafe extern "system" fn() -> isize, F>(&f) })
}

/// A successful `nvmlInit_v2`. Drop calls `nvmlShutdown`, then frees the DLL.
struct NvmlSession {
    shutdown: NoArgFn,
    /// Declared last so it drops after `Drop::drop` has called `shutdown`.
    lib: Library,
}

impl Drop for NvmlSession {
    fn drop(&mut self) {
        // SAFETY: init succeeded exactly once for this session, so one shutdown balances it; the DLL is still
        // loaded because `lib` drops after this function returns.
        let _ = unsafe { (self.shutdown)() };
    }
}

/// NVML loaded at runtime (no link-time dependency), with GPU 0 open.
struct Nvml {
    device: NvmlDevice,
    name: String,
    utilization: Option<UtilizationFn>,
    encoder: Option<CodecFn>,
    decoder: Option<CodecFn>,
    clock: Option<SelectFn>,
    power: Option<PowerFn>,
    temperature: Option<SelectFn>,
    /// Declared last: the function pointers above are never used after it drops.
    _session: NvmlSession,
}

fn load_nvml() -> Option<Library> {
    // SAFETY: a static NUL-terminated wide string; SEARCH_SYSTEM32 loads only from System32.
    let lib = unsafe { LoadLibraryExW(w!("nvml.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.or_else(|_| {
        // SAFETY: an absolute static path; its dependencies come from its own folder or System32 only.
        unsafe {
            LoadLibraryExW(
                w!("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll"),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
    });
    lib.ok().map(Library)
}

impl Nvml {
    /// Load NVML, init it and open GPU 0. None when any of that fails (no NVIDIA driver, no GPU).
    fn open() -> Option<Self> {
        let lib = load_nvml()?;
        // SAFETY: `nvmlReturn_t nvmlInit_v2(void)`.
        let init: NoArgFn = unsafe { symbol(&lib, s!("nvmlInit_v2")) }?;
        // SAFETY: `nvmlReturn_t nvmlShutdown(void)`.
        let shutdown: NoArgFn = unsafe { symbol(&lib, s!("nvmlShutdown")) }?;
        // SAFETY: `nvmlReturn_t nvmlDeviceGetHandleByIndex_v2(unsigned int, nvmlDevice_t*)`.
        let by_index: HandleByIndexFn = unsafe { symbol(&lib, s!("nvmlDeviceGetHandleByIndex_v2")) }?;
        // SAFETY: nvmlInit_v2 has no preconditions; the DLL is loaded.
        if unsafe { init() } != NVML_SUCCESS {
            return None;
        }
        // From here on, dropping `session` shuts NVML down and frees the DLL.
        let session = NvmlSession { shutdown, lib };

        let mut device: NvmlDevice = null_mut();
        // SAFETY: NVML is initialised and `device` is a valid out-pointer.
        if unsafe { by_index(0, &raw mut device) } != NVML_SUCCESS || device.is_null() {
            return None;
        }

        let lib = &session.lib;
        // SAFETY (all lookups below): each type matches the NVML header declaration, and the pointers live in
        // `Nvml`, which drops them before `_session` frees the DLL.
        // SAFETY: `nvmlDeviceGetName(nvmlDevice_t, char*, unsigned int)`.
        let get_name: Option<NameFn> = unsafe { symbol(lib, s!("nvmlDeviceGetName")) };
        // SAFETY: `nvmlDeviceGetUtilizationRates(nvmlDevice_t, nvmlUtilization_t*)`.
        let utilization = unsafe { symbol(lib, s!("nvmlDeviceGetUtilizationRates")) };
        // SAFETY: `nvmlDeviceGetEncoderUtilization(nvmlDevice_t, unsigned int*, unsigned int*)`.
        let encoder = unsafe { symbol(lib, s!("nvmlDeviceGetEncoderUtilization")) };
        // SAFETY: `nvmlDeviceGetDecoderUtilization(nvmlDevice_t, unsigned int*, unsigned int*)`.
        let decoder = unsafe { symbol(lib, s!("nvmlDeviceGetDecoderUtilization")) };
        // SAFETY: `nvmlDeviceGetClockInfo(nvmlDevice_t, nvmlClockType_t, unsigned int*)`; the enum is an int.
        let clock = unsafe { symbol(lib, s!("nvmlDeviceGetClockInfo")) };
        // SAFETY: `nvmlDeviceGetPowerUsage(nvmlDevice_t, unsigned int*)`.
        let power = unsafe { symbol(lib, s!("nvmlDeviceGetPowerUsage")) };
        // SAFETY: `nvmlDeviceGetTemperature(nvmlDevice_t, nvmlTemperatureSensors_t, unsigned int*)`.
        let temperature = unsafe { symbol(lib, s!("nvmlDeviceGetTemperature")) };

        let name = get_name.and_then(|f| device_name(f, device)).unwrap_or_else(|| "NVIDIA GPU".to_owned());
        Some(Nvml { device, name, utilization, encoder, decoder, clock, power, temperature, _session: session })
    }

    fn read(&self) -> GpuReading {
        let d = self.device;
        let core_pct = self.utilization.and_then(|f| {
            let mut u = NvmlUtilization::default();
            // SAFETY: `d` is GPU 0 of the live session and `u` has the C layout of nvmlUtilization_t.
            (unsafe { f(d, &raw mut u) } == NVML_SUCCESS).then(|| f64::from(u.gpu))
        });
        let codec = |f: CodecFn| {
            let (mut pct, mut period) = (0u32, 0u32);
            // SAFETY: `d` is GPU 0 of the live session; both out-pointers are valid locals.
            (unsafe { f(d, &raw mut pct, &raw mut period) } == NVML_SUCCESS).then(|| f64::from(pct))
        };
        let select = |f: SelectFn, which: i32| {
            let mut v = 0u32;
            // SAFETY: `d` is GPU 0 of the live session; `which` is a valid enum value; `v` is a valid out-pointer.
            (unsafe { f(d, which, &raw mut v) } == NVML_SUCCESS).then(|| f64::from(v))
        };
        let power_w = self.power.and_then(|f| {
            let mut mw = 0u32;
            // SAFETY: `d` is GPU 0 of the live session; `mw` is a valid out-pointer.
            (unsafe { f(d, &raw mut mw) } == NVML_SUCCESS).then(|| f64::from(mw) / 1000.0)
        });
        GpuReading {
            name: self.name.clone(),
            source: "nvml",
            core_pct,
            encoder_pct: self.encoder.and_then(codec),
            decoder_pct: self.decoder.and_then(codec),
            clock_mhz: self.clock.and_then(|f| select(f, NVML_CLOCK_GRAPHICS)),
            power_w,
            temp_c: self.temperature.and_then(|f| select(f, NVML_TEMPERATURE_GPU)),
        }
    }
}

fn device_name(f: NameFn, device: NvmlDevice) -> Option<String> {
    let mut buf = [0u8; NVML_NAME_LEN];
    let len = u32::try_from(buf.len()).ok()?;
    // SAFETY: `device` is a live handle; `buf` is writable for `len` bytes and NVML NUL-terminates within it.
    if unsafe { f(device, buf.as_mut_ptr(), len) } != NVML_SUCCESS {
        return None;
    }
    let name = CStr::from_bytes_until_nul(&buf).ok()?.to_string_lossy().trim().to_owned();
    (!name.is_empty()).then_some(name)
}

// ------------------------------------------------------------------------------------------------ Wi-Fi

/// WLAN client version 2 (Vista and later).
const WLAN_CLIENT_VERSION: u32 = 2;

/// A handle from `WlanOpenHandle`, closed on drop.
struct WlanClient(HANDLE);

impl Drop for WlanClient {
    fn drop(&mut self) {
        // SAFETY: the handle came from WlanOpenHandle and is closed only here.
        let _ = unsafe { WlanCloseHandle(self.0, None) };
    }
}

/// Types that are valid for every bit pattern, so they can be read from a WLAN buffer as is.
///
/// # Safety
/// Every bit pattern of `size_of::<Self>()` bytes must be a valid `Self`.
unsafe trait Plain: Copy {}
// SAFETY: every bit pattern is a valid u32.
unsafe impl Plain for u32 {}
// SAFETY: every bit pattern is a valid i32.
unsafe impl Plain for i32 {}
// SAFETY: every bit pattern is a valid byte array.
unsafe impl Plain for [u8; 6] {}
// SAFETY: GUID is four integer fields (u32, u16, u16, [u8; 8]); every bit pattern is valid.
unsafe impl Plain for GUID {}

/// A buffer the WLAN API allocated. Freed with `WlanFreeMemory` on drop.
///
/// Fields are read one by one at checked offsets instead of as whole structs: no SSID or profile name is ever
/// copied out, and `WLAN_BSS_ENTRY` has a Rust `bool` field that must not be read from foreign memory.
struct WlanBuf {
    ptr: NonNull<u8>,
    /// Bytes known to be readable.
    len: usize,
}

impl WlanBuf {
    /// # Safety
    /// `ptr` must be null or a buffer the WLAN API returned to this caller, readable for `len` bytes, that no one
    /// else frees.
    unsafe fn new(ptr: *mut c_void, len: usize) -> Option<Self> {
        NonNull::new(ptr.cast::<u8>()).map(|ptr| WlanBuf { ptr, len })
    }

    /// Widen the readable length, from a size the buffer itself reports.
    ///
    /// # Safety
    /// The buffer must really be readable for `len` bytes.
    unsafe fn assume_len(&mut self, len: usize) {
        self.len = len;
    }

    /// The `T` at `offset`, or None when it does not fit in the buffer.
    fn get<T: Plain>(&self, offset: usize) -> Option<T> {
        let end = offset.checked_add(size_of::<T>())?;
        if end > self.len {
            return None;
        }
        let p = self.ptr.as_ptr().wrapping_add(offset).cast::<T>();
        // SAFETY: `offset..end` lies inside the readable buffer (checked above); `T: Plain` accepts any bits;
        // read_unaligned has no alignment need.
        Some(unsafe { p.read_unaligned() })
    }
}

impl Drop for WlanBuf {
    fn drop(&mut self) {
        // SAFETY: the WLAN API allocated this buffer for us and it is freed only here.
        unsafe { WlanFreeMemory(self.ptr.as_ptr().cast_const().cast()) };
    }
}

/// Plain-words reason for a WLAN error code.
fn wlan_reason(e: WIN32_ERROR) -> String {
    match e {
        ERROR_ACCESS_DENIED => "Windows blocks Wi-Fi details: turn on Location for desktop apps".to_owned(),
        ERROR_SERVICE_NOT_ACTIVE | ERROR_SERVICE_DISABLED | ERROR_SERVICE_DOES_NOT_EXIST => {
            "The Wi-Fi service (WLAN AutoConfig) is not running".to_owned()
        }
        e => format!("Windows could not read Wi-Fi details (error {})", e.0),
    }
}

fn win32(code: u32) -> Result<(), WIN32_ERROR> {
    if code == 0 {
        Ok(())
    } else {
        Err(WIN32_ERROR(code))
    }
}

/// Result of looking for a connected Wi-Fi interface.
enum Interface {
    Connected(GUID),
    NoneConnected,
    NoAdapter,
}

impl WlanClient {
    fn open() -> Result<Self, WIN32_ERROR> {
        let mut version = 0u32;
        let mut handle = HANDLE::default();
        // SAFETY: both out-pointers are valid locals; the reserved pointer is null as required.
        win32(unsafe { WlanOpenHandle(WLAN_CLIENT_VERSION, None, &raw mut version, &raw mut handle) })?;
        Ok(WlanClient(handle))
    }

    fn read(&self) -> Wifi {
        let guid = match self.connected_interface() {
            Ok(Interface::Connected(g)) => g,
            Ok(Interface::NoneConnected) => return Wifi::NotConnected,
            Ok(Interface::NoAdapter) => return Wifi::Unavailable { reason: "No Wi-Fi adapter".to_owned() },
            Err(e) => return Wifi::Unavailable { reason: wlan_reason(e) },
        };

        let conn = match self.query(&guid, wlan_intf_opcode_current_connection) {
            Ok(b) => b,
            Err(e) => return Wifi::Unavailable { reason: wlan_reason(e) },
        };
        let assoc = offset_of!(WLAN_CONNECTION_ATTRIBUTES, wlanAssociationAttributes);
        let field = |off: usize| assoc + off;
        let phy_type: Option<i32> = conn.get(field(offset_of!(WLAN_ASSOCIATION_ATTRIBUTES, dot11PhyType)));
        let signal: Option<u32> = conn.get(field(offset_of!(WLAN_ASSOCIATION_ATTRIBUTES, wlanSignalQuality)));
        let rx_kbps: Option<u32> = conn.get(field(offset_of!(WLAN_ASSOCIATION_ATTRIBUTES, ulRxRate)));
        let tx_kbps: Option<u32> = conn.get(field(offset_of!(WLAN_ASSOCIATION_ATTRIBUTES, ulTxRate)));
        // In memory only, to find the BSS entry; never stored or returned.
        let bssid: Option<[u8; 6]> = conn.get(field(offset_of!(WLAN_ASSOCIATION_ATTRIBUTES, dot11Bssid)));
        drop(conn);

        let channel = self.query(&guid, wlan_intf_opcode_channel_number).ok().and_then(|b| b.get::<u32>(0));
        let rssi_dbm = self.query(&guid, wlan_intf_opcode_rssi).ok().and_then(|b| b.get::<i32>(0));
        let frequency_mhz = bssid.and_then(|b| self.bss_frequency_mhz(&guid, b));

        Wifi::Connected(WifiLink {
            band: band(frequency_mhz, channel).map(str::to_owned),
            channel: channel.filter(|&c| c != 0),
            frequency_mhz,
            phy: phy_type.and_then(phy_name),
            rx_mbps: rx_kbps.map(kbps_to_mbps),
            tx_mbps: tx_kbps.map(kbps_to_mbps),
            signal_pct: signal.map(|s| s.min(100)),
            rssi_dbm,
        })
    }

    /// The first interface in the connected state.
    fn connected_interface(&self) -> Result<Interface, WIN32_ERROR> {
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
        // SAFETY: the client handle is open; `list` is a valid out-pointer; the reserved pointer is null.
        win32(unsafe { WlanEnumInterfaces(self.0, None, &raw mut list) })?;
        let header = offset_of!(WLAN_INTERFACE_INFO_LIST, InterfaceInfo);
        // SAFETY: on success the API returns a list it allocated for us; its header is always present.
        let Some(mut buf) = (unsafe { WlanBuf::new(list.cast(), header) }) else {
            return Ok(Interface::NoAdapter);
        };
        let count = buf.get::<u32>(offset_of!(WLAN_INTERFACE_INFO_LIST, dwNumberOfItems)).unwrap_or(0);
        let count = usize::try_from(count).unwrap_or(0);
        if count == 0 {
            return Ok(Interface::NoAdapter);
        }
        let item = size_of::<WLAN_INTERFACE_INFO>();
        let Some(len) = count.checked_mul(item).and_then(|n| n.checked_add(header)) else {
            return Ok(Interface::NoAdapter);
        };
        // SAFETY: the list holds `dwNumberOfItems` entries in its flexible array `InterfaceInfo`.
        unsafe { buf.assume_len(len) };
        for i in 0..count {
            let base = header + i * item;
            let state = buf.get::<i32>(base + offset_of!(WLAN_INTERFACE_INFO, isState));
            if state == Some(wlan_interface_state_connected.0) {
                if let Some(guid) = buf.get::<GUID>(base + offset_of!(WLAN_INTERFACE_INFO, InterfaceGuid)) {
                    return Ok(Interface::Connected(guid));
                }
            }
        }
        Ok(Interface::NoneConnected)
    }

    fn query(&self, guid: &GUID, opcode: WLAN_INTF_OPCODE) -> Result<WlanBuf, WIN32_ERROR> {
        let mut size = 0u32;
        let mut data: *mut c_void = null_mut();
        // SAFETY: the client handle is open; `guid` outlives the call; `size` and `data` are valid out-pointers.
        win32(unsafe { WlanQueryInterface(self.0, guid, opcode, None, &raw mut size, &raw mut data, None) })?;
        let len = usize::try_from(size).unwrap_or(0);
        // SAFETY: on success `data` is a buffer the API allocated for us, readable for `size` bytes.
        unsafe { WlanBuf::new(data, len) }.ok_or(WIN32_ERROR(0x8000_4005))
    }

    /// Center frequency of the BSS we are connected to, from the cached BSS list.
    fn bss_frequency_mhz(&self, guid: &GUID, bssid: [u8; 6]) -> Option<u32> {
        let mut list: *mut WLAN_BSS_LIST = null_mut();
        // SAFETY: the client handle is open; no SSID filter (null); `list` is a valid out-pointer. The security
        // flag is ignored when no SSID is given.
        win32(unsafe {
            WlanGetNetworkBssList(self.0, guid, None, dot11_BSS_type_infrastructure, false, None, &raw mut list)
        })
        .ok()?;
        let header = offset_of!(WLAN_BSS_LIST, wlanBssEntries);
        // SAFETY: on success the API returns a list it allocated for us; its header is always present.
        let mut buf = unsafe { WlanBuf::new(list.cast(), header) }?;
        let total = usize::try_from(buf.get::<u32>(offset_of!(WLAN_BSS_LIST, dwTotalSize))?).ok()?;
        let count = usize::try_from(buf.get::<u32>(offset_of!(WLAN_BSS_LIST, dwNumberOfItems))?).ok()?;
        // SAFETY: `dwTotalSize` is the size of the whole list in bytes, as the API reports it.
        unsafe { buf.assume_len(total.max(header)) };
        let item = size_of::<WLAN_BSS_ENTRY>();
        (0..count).find_map(|i| {
            let base = header.checked_add(i.checked_mul(item)?)?;
            if buf.get::<[u8; 6]>(base + offset_of!(WLAN_BSS_ENTRY, dot11Bssid))? != bssid {
                return None;
            }
            let khz = buf.get::<u32>(base + offset_of!(WLAN_BSS_ENTRY, ulChCenterFrequency))?;
            Some(khz / 1000).filter(|&mhz| mhz != 0)
        })
    }
}

fn kbps_to_mbps(kbps: u32) -> f64 {
    f64::from(kbps) / 1000.0
}

/// Band from the center frequency when known, else from the channel number.
fn band(frequency_mhz: Option<u32>, channel: Option<u32>) -> Option<&'static str> {
    match frequency_mhz {
        Some(mhz) => band_from_mhz(mhz),
        None => channel.and_then(band_from_channel),
    }
}

fn band_from_mhz(mhz: u32) -> Option<&'static str> {
    match mhz {
        2400..=2500 => Some("2.4 GHz"),
        4900..=5924 => Some("5 GHz"),
        5925..=7125 => Some("6 GHz"),
        _ => None,
    }
}

/// 1-14 is 2.4 GHz and 32-177 is 5 GHz. 6 GHz reuses small numbers, so other values are ambiguous: None.
fn band_from_channel(channel: u32) -> Option<&'static str> {
    match channel {
        1..=14 => Some("2.4 GHz"),
        32..=177 => Some("5 GHz"),
        _ => None,
    }
}

/// Name of a `DOT11_PHY_TYPE`. None for unknown (0).
fn phy_name(phy: i32) -> Option<String> {
    let name = match phy {
        0 => return None,
        1 => "802.11 FHSS",
        2 => "802.11 DSSS",
        3 => "802.11 infrared",
        4 => "802.11a",
        5 => "802.11b",
        6 => "802.11g",
        7 => "802.11n (Wi-Fi 4)",
        8 => "802.11ac (Wi-Fi 5)",
        9 => "802.11ad (WiGig)",
        10 => "802.11ax (Wi-Fi 6/6E)",
        11 => "802.11be (Wi-Fi 7)",
        other => return Some(format!("PHY type {other}")),
    };
    Some(name.to_owned())
}

// ------------------------------------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    fn times(idle: u64, kernel: u64, user: u64) -> CpuTimes {
        CpuTimes { idle, kernel, user }
    }

    #[test]
    fn cpu_percent_math() {
        // 1000 ticks total (kernel 600 incl. idle 250, user 400): 75% busy.
        assert!(close(cpu_percent(times(0, 0, 0), times(250, 600, 400)), 75.0));
        assert!(close(cpu_percent(times(10, 10, 10), times(1010, 1010, 10)), 0.0));
        assert!(close(cpu_percent(times(0, 0, 0), times(0, 500, 500)), 100.0));
        // No time passed, or counters went backwards.
        assert_eq!(cpu_percent(times(5, 5, 5), times(5, 5, 5)), None);
        assert_eq!(cpu_percent(times(5, 5, 5), times(4, 9, 9)), None);
        // Idle larger than total (should not happen) clamps to 0, not negative.
        assert!(close(cpu_percent(times(0, 0, 0), times(900, 100, 100)), 0.0));
        assert_eq!(filetime_ticks(FILETIME { dwLowDateTime: 2, dwHighDateTime: 1 }), (1 << 32) | 2);
    }

    #[test]
    fn smi_line_parses() {
        let g = parse_smi_line("NVIDIA GeForce RTX 4070 Laptop GPU, 12, 3, 0, 2100, 35.47, 61\r").expect("parses");
        assert_eq!(g.name, "NVIDIA GeForce RTX 4070 Laptop GPU");
        assert_eq!(g.source, "nvidia-smi");
        assert!(close(g.core_pct, 12.0));
        assert!(close(g.encoder_pct, 3.0));
        assert!(close(g.decoder_pct, 0.0));
        assert!(close(g.clock_mhz, 2100.0));
        assert!(close(g.power_w, 35.47));
        assert!(close(g.temp_c, 61.0));
    }

    #[test]
    fn smi_line_na_and_bad_input() {
        let g = parse_smi_line("Quadro, Pro, 5, [N/A], [N/A], [Not Supported], [N/A], 40").expect("parses");
        assert_eq!(g.name, "Quadro, Pro");
        assert!(close(g.core_pct, 5.0));
        assert_eq!((g.encoder_pct, g.decoder_pct, g.clock_mhz, g.power_w), (None, None, None, None));
        assert!(close(g.temp_c, 40.0));
        assert_eq!(parse_smi_line(""), None);
        assert_eq!(parse_smi_line("1, 2, 3"), None);
        assert_eq!(parse_smi_line(" , 1, 2, 3, 4, 5, 6"), None);
    }

    #[test]
    fn bands() {
        assert_eq!(band(Some(2437), Some(6)), Some("2.4 GHz"));
        assert_eq!(band(Some(5180), None), Some("5 GHz"));
        assert_eq!(band(Some(5975), Some(1)), Some("6 GHz"));
        assert_eq!(band(None, Some(11)), Some("2.4 GHz"));
        assert_eq!(band(None, Some(149)), Some("5 GHz"));
        assert_eq!(band(None, Some(181)), None);
        assert_eq!(band(None, Some(0)), None);
        assert_eq!(band(None, None), None);
        assert_eq!(band(Some(60_480), None), None);
    }

    #[test]
    fn phy_names() {
        assert_eq!(phy_name(0), None);
        assert_eq!(phy_name(4).as_deref(), Some("802.11a"));
        assert_eq!(phy_name(5).as_deref(), Some("802.11b"));
        assert_eq!(phy_name(6).as_deref(), Some("802.11g"));
        assert_eq!(phy_name(7).as_deref(), Some("802.11n (Wi-Fi 4)"));
        assert_eq!(phy_name(8).as_deref(), Some("802.11ac (Wi-Fi 5)"));
        assert_eq!(phy_name(10).as_deref(), Some("802.11ax (Wi-Fi 6/6E)"));
        assert_eq!(phy_name(11).as_deref(), Some("802.11be (Wi-Fi 7)"));
        assert_eq!(phy_name(42).as_deref(), Some("PHY type 42"));
    }

    #[test]
    fn wifi_reasons_and_default() {
        assert_eq!(wlan_reason(ERROR_ACCESS_DENIED), "Windows blocks Wi-Fi details: turn on Location for desktop apps");
        assert!(wlan_reason(ERROR_SERVICE_NOT_ACTIVE).contains("not running"));
        assert!(matches!(Wifi::default(), Wifi::Unavailable { .. }));
        assert!(close(Some(kbps_to_mbps(866_700)), 866.7));
    }

    #[test]
    fn wlan_buf_bounds() {
        // A buffer that is not WLAN memory must never be dropped, so check `get` on a borrowed view only.
        let bytes = [1u8, 0, 0, 0, 2, 0, 0, 0];
        let view = std::mem::ManuallyDrop::new(WlanBuf { ptr: NonNull::from(&bytes).cast::<u8>(), len: bytes.len() });
        assert_eq!(view.get::<u32>(0), Some(1));
        assert_eq!(view.get::<u32>(4), Some(2));
        assert_eq!(view.get::<u32>(5), None);
        assert_eq!(view.get::<u32>(usize::MAX), None);
        assert_eq!(view.get::<[u8; 6]>(2), Some([0, 0, 2, 0, 0, 0]));
    }

    /// Reads this PC twice and prints the second reading. Run with `--nocapture` to see it.
    #[test]
    fn live_reading() {
        let mut probe = PcProbe::new();
        let _ = probe.read();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let r = probe.read();
        println!("{probe:?}\n{}", serde_json::to_string_pretty(&r).expect("serializes"));
        assert!(r.cpu_pct.is_some_and(|c| (0.0..=100.0).contains(&c)));
        assert!(local_time().year >= 2024);

        let start = std::time::Instant::now();
        for _ in 0..10 {
            let _ = probe.read();
        }
        println!("read() average: {:?}", start.elapsed() / 10);
        // The fallback path, run directly (None on a PC without an NVIDIA driver).
        println!("nvidia-smi fallback: {:?}", smi_read());
    }
}
