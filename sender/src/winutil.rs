//! Windows helpers: per-app GPU preference, helper hardlinks, timer resolution
//! (own process and child ffmpeg), power-throttling opt-out, child CPU time and thread priority.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows::core::{HSTRING, PCSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::ProcessStatus::{K32EnumProcessModules, K32GetModuleBaseNameW};
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, GetProcessTimes, OpenThread, ProcessPowerThrottling, SetProcessInformation,
    SetThreadPriority, WaitForSingleObject, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, THREAD_PRIORITY, THREAD_SET_INFORMATION,
};

/// GPU preference values used by Settings > System > Display > Graphics.
pub const GPU_PREF_POWER_SAVING: u32 = 1;
pub const GPU_PREF_HIGH_PERFORMANCE: u32 = 2;

/// `size_of::<T>()` as the `u32` Win32 size fields and byte counts want.
fn size_u32<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).expect("Win32 struct size fits u32")
}

/// Set the per-user, per-exe GPU preference (HKCU\Software\Microsoft\DirectX\UserGpuPreferences).
/// Read by DXGI at process start; decides which adapter "owns" the outputs on a hybrid laptop.
///
/// # Errors
///
/// When the registry value cannot be written.
pub fn set_gpu_preference(exe: &Path, pref: u32) -> anyhow::Result<()> {
    let name = HSTRING::from(exe.as_os_str());
    let val: Vec<u16> = format!("GpuPreference={pref};").encode_utf16().chain(Some(0)).collect();
    let val_bytes = u32::try_from(val.len() * 2)?;
    // SAFETY: `name` and the key path are live HSTRINGs; `val` is a NUL-terminated UTF-16 buffer
    // that outlives the call, and the byte length passed matches its size.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from("Software\\Microsoft\\DirectX\\UserGpuPreferences"),
            &name,
            REG_SZ.0,
            Some(val.as_ptr().cast::<c_void>()),
            val_bytes,
        )
    };
    if r.is_err() {
        anyhow::bail!("RegSetKeyValueW failed: {r:?}");
    }
    Ok(())
}

/// Make `dst` a hardlink (or copy, if linking fails) of `src`. Skips work when already linked.
///
/// # Errors
///
/// When both the hardlink and the copy fail.
pub fn ensure_link(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if let (Ok(a), Ok(b)) = (std::fs::metadata(src), std::fs::metadata(dst)) {
        if a.len() == b.len() && a.modified().ok() == b.modified().ok() {
            return Ok(());
        }
    }
    let _ = std::fs::remove_file(dst);
    if std::fs::hard_link(src, dst).is_err() {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Find an executable on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let p = d.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 1 ms timer resolution for this process (Windows 11 applies it per process).
pub fn high_res_timer_self() {
    // SAFETY: timeBeginPeriod takes a plain integer.
    unsafe { timeBeginPeriod(1) };
    // The current-process pseudo-handle, -1 (same bits as the old `-1isize as *mut c_void`).
    let me = HANDLE(std::ptr::without_provenance_mut::<c_void>(usize::MAX));
    // SAFETY: -1 is the documented pseudo-handle for the current process, valid for the whole
    // call and with every access right.
    unsafe { disable_throttling(me) };
}

/// # Safety
/// `h` must be a valid process handle (or the current-process pseudo-handle) with
/// `PROCESS_SET_INFORMATION` access for the duration of the call.
unsafe fn disable_throttling(h: HANDLE) {
    // ControlMask set + StateMask clear = "never throttle" and "always honor timer resolution".
    let st = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: `h` is valid per this function's contract; `st` is a correctly sized, initialized
    // PROCESS_POWER_THROTTLING_STATE on our stack that lives until the call returns.
    let _ = unsafe {
        SetProcessInformation(
            h,
            ProcessPowerThrottling,
            (&raw const st).cast::<c_void>(),
            size_u32::<PROCESS_POWER_THROTTLING_STATE>(),
        )
    };
}

/// Give a child process (ffmpeg) 1 ms timer resolution by running `timeBeginPeriod(1)` on a
/// remote thread inside it. ffmpeg's ddagrab paces frames with `Sleep()`; at the default
/// 15.6 ms timer granularity it cannot hold 60 fps (measured ~50 fps).
///
/// `timeBeginPeriod(UINT)` matches the thread start signature on x64, and winmm.dll is mapped at
/// the same base in every process of a boot session (we verify that before calling).
///
/// # Errors
///
/// When winmm.dll or `timeBeginPeriod` cannot be found here or in the child, the module bases
/// differ, or the remote thread cannot be created, does not finish in 2 s, or fails.
pub fn high_res_timer_child(child: &std::process::Child) -> anyhow::Result<()> {
    // `h` is the child's process handle, owned by `child` (borrowed for this call, so it stays
    // open, with all access rights).
    let h = HANDLE(child.as_raw_handle());
    // SAFETY: `h` is a valid process handle with PROCESS_SET_INFORMATION access (see above).
    unsafe { disable_throttling(h) };
    // SAFETY: the library name is a live HSTRING for the call.
    let ours = unsafe { LoadLibraryW(&HSTRING::from("winmm.dll")) }?;
    // SAFETY: `ours` is a loaded module handle; the name is a NUL-terminated C string literal.
    let f = unsafe { GetProcAddress(ours, PCSTR(c"timeBeginPeriod".as_ptr().cast())) }
        .ok_or_else(|| anyhow::anyhow!("no timeBeginPeriod"))?;
    // Wait for the loader to map winmm.dll in the child (static import of ffmpeg.exe).
    let mut remote_base = None;
    for _ in 0..200 {
        // SAFETY: `h` is valid with PROCESS_QUERY_INFORMATION | PROCESS_VM_READ access (see above).
        remote_base = unsafe { find_module(h, "winmm.dll") };
        if remote_base.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let rb = remote_base.ok_or_else(|| anyhow::anyhow!("winmm.dll not loaded in child"))?;
    if rb.0 != ours.0 {
        anyhow::bail!("winmm.dll base differs in child ({:?} vs {:?})", rb.0, ours.0);
    }
    // SAFETY: turns `timeBeginPeriod(UINT) -> MMRESULT` into a thread start routine
    // `fn(*mut c_void) -> u32`: on x64 both take one register argument and return in eax. It is
    // only called in the child, where winmm.dll is mapped at the same base as here (checked
    // just above), so the address is valid code there.
    let start: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(f) };
    // SAFETY: `h` is a valid process handle with thread-creation rights; `start` is valid code in
    // the child (see above) and its argument is the plain integer 1 (uPeriod = 1 ms).
    let th = unsafe {
        CreateRemoteThread(h, None, 0, Some(start), Some(std::ptr::without_provenance::<c_void>(1)), 0, None)
    }?;
    // SAFETY: `th` is the live thread handle returned just above; it is closed below, once.
    let ok = unsafe { WaitForSingleObject(th, 2000) } == WAIT_OBJECT_0;
    let mut code = 0u32;
    // SAFETY: `th` is a live thread handle; `code` is a live local out-param.
    let _ = unsafe { GetExitCodeThread(th, &raw mut code) };
    // SAFETY: `th` is a live handle owned here and not used after this line.
    let _ = unsafe { CloseHandle(th) };
    if !ok || code != 0 {
        anyhow::bail!("remote timeBeginPeriod: wait_ok={ok} code={code}");
    }
    Ok(())
}

/// # Safety
/// `h` must be a valid process handle with `PROCESS_QUERY_INFORMATION` | `PROCESS_VM_READ` access.
unsafe fn find_module(h: HANDLE, name: &str) -> Option<HMODULE> {
    let mut mods = vec![HMODULE::default(); 1024];
    let mut needed = 0u32;
    let mods_bytes = u32::try_from(mods.len() * std::mem::size_of::<HMODULE>()).ok()?;
    // SAFETY: `h` is valid per the contract; `mods` has room for the byte count we pass.
    unsafe { K32EnumProcessModules(h, mods.as_mut_ptr(), mods_bytes, &raw mut needed) }.ok().ok()?;
    let needed = usize::try_from(needed).unwrap_or(usize::MAX);
    let n = (needed / std::mem::size_of::<HMODULE>()).min(mods.len());
    let mut buf = [0u16; 260];
    for m in &mods[..n] {
        // SAFETY: `h` is valid per the contract; `buf` is a live writable slice (length passed by
        // the binding), and the returned length never exceeds it.
        let len = unsafe { K32GetModuleBaseNameW(h, Some(*m), &mut buf) };
        let len = usize::try_from(len).unwrap_or(0);
        if String::from_utf16_lossy(&buf[..len]).eq_ignore_ascii_case(name) {
            return Some(*m);
        }
    }
    None
}

/// User + kernel CPU time of a child process, microseconds.
pub fn cpu_time_us(child: &std::process::Child) -> Option<u64> {
    let h = HANDLE(child.as_raw_handle());
    let (mut created, mut exited, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: `h` is the child's process handle, kept open by the borrowed `child`; the four
    // FILETIME out-params are live locals.
    unsafe { GetProcessTimes(h, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user).ok()? };
    let us = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)) / 10;
    Some(us(kernel) + us(user))
}

/// Set the priority of every thread of process `pid` (e.g. `THREAD_PRIORITY_HIGHEST` = 2,
/// `TIME_CRITICAL` = 15). Returns how many threads were changed. Threads started later keep the
/// default priority, so call this again after the pipeline is up.
pub fn set_thread_priorities(pid: u32, prio: i32) -> usize {
    let mut n = 0;
    // SAFETY: plain value arguments; the snapshot handle is closed once, below.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else { return 0 };
    let mut te = THREADENTRY32 { dwSize: size_u32::<THREADENTRY32>(), ..Default::default() };
    // SAFETY: `snap` is a live thread snapshot; `te` is a live local with the correct dwSize.
    let mut ok = unsafe { Thread32First(snap, &raw mut te) }.is_ok();
    while ok {
        if te.th32OwnerProcessID == pid {
            // SAFETY: plain value arguments; the returned handle is closed once, below.
            if let Ok(th) = unsafe { OpenThread(THREAD_SET_INFORMATION, false, te.th32ThreadID) } {
                // SAFETY: `th` is a live thread handle opened with THREAD_SET_INFORMATION.
                if unsafe { SetThreadPriority(th, THREAD_PRIORITY(prio)) }.is_ok() {
                    n += 1;
                }
                // SAFETY: `th` is a live handle owned here and not used after this line.
                let _ = unsafe { CloseHandle(th) };
            }
        }
        // SAFETY: `snap` is a live thread snapshot; `te` is a live local with the correct dwSize.
        ok = unsafe { Thread32Next(snap, &raw mut te) }.is_ok();
    }
    // SAFETY: `snap` is a live handle owned here and not used after this line.
    let _ = unsafe { CloseHandle(snap) };
    n
}

/// Terminate a process by id (best effort).
pub fn kill_pid(pid: u32) {
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: plain value arguments; the returned handle is closed once, below.
    if let Ok(h) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        // SAFETY: `h` is a live process handle opened with PROCESS_TERMINATE.
        let _ = unsafe { TerminateProcess(h, 1) };
        // SAFETY: `h` is a live handle owned here and not used after this line.
        let _ = unsafe { CloseHandle(h) };
    }
}

/// The process `pid` is still running (false for pid 0 or one that cannot be opened).
/// What is known about process `pid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    /// It exited, or no process has that id.
    Gone,
    /// It exists but cannot be inspected (e.g. an elevated process started with another administrator's
    /// credentials): treat it as running.
    Unknown,
}

/// The state of process `pid`. An `OpenProcess` failure other than "no such process" (invalid parameter) is
/// [`ProcessState::Unknown`], never [`ProcessState::Gone`].
pub fn process_state(pid: u32) -> ProcessState {
    use windows::Win32::Foundation::WAIT_TIMEOUT;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
    if pid == 0 {
        return ProcessState::Gone;
    }
    // SAFETY: plain value arguments; the returned handle is closed once, below.
    let h = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(h) => h,
        Err(e) => return state_of_open_error(e.code()),
    };
    // SAFETY: `h` is a live process handle opened with SYNCHRONIZE; a zero wait only polls.
    let running = unsafe { WaitForSingleObject(h, 0) } == WAIT_TIMEOUT;
    // SAFETY: `h` is owned here and not used after this line.
    let _ = unsafe { CloseHandle(h) };
    if running {
        ProcessState::Running
    } else {
        ProcessState::Gone
    }
}

/// `OpenProcess` failed: "invalid parameter" means no such process; anything else (access denied) does not.
fn state_of_open_error(code: windows::core::HRESULT) -> ProcessState {
    if code == windows::Win32::Foundation::ERROR_INVALID_PARAMETER.to_hresult() {
        ProcessState::Gone
    } else {
        ProcessState::Unknown
    }
}

pub fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::WAIT_TIMEOUT;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
    if pid == 0 {
        return false;
    }
    // SAFETY: plain value arguments; the returned handle is closed once, below.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else { return false };
    // SAFETY: `h` is a live process handle opened with SYNCHRONIZE; a zero wait only polls.
    let running = unsafe { WaitForSingleObject(h, 0) } == WAIT_TIMEOUT;
    // SAFETY: `h` is owned here and not used after this line.
    let _ = unsafe { CloseHandle(h) };
    running
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[test]
    fn an_inaccessible_process_is_not_gone() {
        use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
        assert_eq!(state_of_open_error(ERROR_INVALID_PARAMETER.to_hresult()), ProcessState::Gone);
        assert_eq!(
            state_of_open_error(ERROR_ACCESS_DENIED.to_hresult()),
            ProcessState::Unknown,
            "access denied: it may still run"
        );
        assert_eq!(process_state(std::process::id()), ProcessState::Running);
        assert_eq!(process_state(0), ProcessState::Gone);
    }
}
