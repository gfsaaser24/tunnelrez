//! Voice priority for the audio datagrams (qWAVE, as Sunshine does): Windows marks the socket's packets with the
//! DSCP value of `QOSTrafficTypeVoice` (56), which Wi-Fi access points map to WMM `AC_VO`, the highest access
//! category. Without it the 5 ms audio datagrams queue behind large video frames when the picture starts moving.
//!
//! Windows ignores `IP_TOS` from programs (and qWAVE's own `QOSSetOutgoingDSCPValue` needs admin rights), so
//! this is the way a normal process gets its packets tagged. qwave.dll is loaded at run time: it is missing on
//! some Windows Server installs, and everything works (untagged) without it.

use std::ffi::c_void;
use std::os::windows::io::AsRawSocket;
use windows::core::{s, w};
use windows::Win32::Foundation::{GetLastError, HANDLE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

/// `QOS_VERSION`.
#[repr(C)]
struct QosVersion {
    major: u16,
    minor: u16,
}

/// `QOSTrafficTypeVoice` (`QOS_TRAFFIC_TYPE`).
const TRAFFIC_TYPE_VOICE: i32 = 4;
/// `QOS_NON_ADAPTIVE_FLOW`: tag at once, do not wait for qWAVE's network probing.
const NON_ADAPTIVE_FLOW: u32 = 2;

/// What `GetProcAddress` returns.
type Proc = unsafe extern "system" fn() -> isize;
type CreateHandle = unsafe extern "system" fn(*const QosVersion, *mut HANDLE) -> i32;
type CloseHandle = unsafe extern "system" fn(HANDLE) -> i32;
type AddSocketToFlow = unsafe extern "system" fn(HANDLE, usize, *const c_void, i32, u32, *mut u32) -> i32;
type RemoveSocketFromFlow = unsafe extern "system" fn(HANDLE, usize, u32, u32) -> i32;

/// A socket in a qWAVE voice flow. Dropping it takes the socket out of the flow and closes the qWAVE handle; drop
/// it before the socket.
#[derive(Debug)]
pub struct VoiceFlow {
    handle: HANDLE,
    flow: u32,
    remove: RemoveSocketFromFlow,
    close: CloseHandle,
}

impl VoiceFlow {
    /// Put a **connected** UDP socket's traffic in a voice flow.
    ///
    /// # Errors
    ///
    /// qwave.dll or one of its functions is missing, or a qWAVE call failed (with the Windows error).
    pub fn new(sock: &std::net::UdpSocket) -> Result<VoiceFlow, String> {
        // SAFETY: a NUL-terminated wide string literal; the search is limited to System32.
        let dll = unsafe { LoadLibraryExW(w!("qwave.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|e| format!("qwave.dll not available: {e}"))?;
        let find = |name| {
            // SAFETY: `dll` is a loaded module (never freed: the process keeps it); `name` is a NUL-terminated
            // C string literal.
            unsafe { GetProcAddress(dll, name) }
        };
        let (Some(create), Some(close), Some(add), Some(remove)) = (
            find(s!("QOSCreateHandle")),
            find(s!("QOSCloseHandle")),
            find(s!("QOSAddSocketToFlow")),
            find(s!("QOSRemoveSocketFromFlow")),
        ) else {
            return Err("qwave.dll lacks the QOS functions".into());
        };
        // SAFETY: the qWAVE export has exactly this signature (qos2.h; WINAPI = system ABI).
        let create = unsafe { std::mem::transmute::<Proc, CreateHandle>(create) };
        // SAFETY: as above.
        let close = unsafe { std::mem::transmute::<Proc, CloseHandle>(close) };
        // SAFETY: as above.
        let add = unsafe { std::mem::transmute::<Proc, AddSocketToFlow>(add) };
        // SAFETY: as above.
        let remove = unsafe { std::mem::transmute::<Proc, RemoveSocketFromFlow>(remove) };
        let version = QosVersion { major: 1, minor: 0 };
        let mut handle = HANDLE::default();
        // SAFETY: both pointers are live locals for the call.
        if unsafe { create(&raw const version, &raw mut handle) } == 0 {
            // SAFETY: plain thread-local error read.
            return Err(format!("QOSCreateHandle failed: {:?}", unsafe { GetLastError() }));
        }
        let socket = usize::try_from(sock.as_raw_socket()).unwrap_or(usize::MAX);
        let mut flow = 0u32;
        // SAFETY: `handle` was just created; `socket` is the live socket behind `sock` (connected, so the
        // destination may be null); `flow` is a live local.
        let ok = unsafe { add(handle, socket, std::ptr::null(), TRAFFIC_TYPE_VOICE, NON_ADAPTIVE_FLOW, &raw mut flow) };
        if ok == 0 {
            // SAFETY: plain thread-local error read.
            let e = unsafe { GetLastError() };
            // SAFETY: the handle created above, closed once.
            unsafe { close(handle) };
            return Err(format!("QOSAddSocketToFlow failed: {e:?}"));
        }
        Ok(VoiceFlow { handle, flow, remove, close })
    }
}

impl Drop for VoiceFlow {
    fn drop(&mut self) {
        // SAFETY: `handle` and `flow` come from a successful `new`; a null socket removes the whole flow
        // (qos2.h), so this is valid even if the socket was already closed. Each is released once, here.
        unsafe { (self.remove)(self.handle, 0, self.flow, 0) };
        // SAFETY: the handle from `new`, closed once, after its flow is gone.
        unsafe { (self.close)(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connected_udp_socket_joins_a_voice_flow() {
        // qWAVE needs a destination it can route (loopback has no interface: ERROR_NOT_FOUND). A documentation
        // address (RFC 5737) goes through the default route; nothing is sent.
        let tx = std::net::UdpSocket::bind(("0.0.0.0", 0)).expect("bind");
        tx.connect(("203.0.113.7", 7878)).expect("connect");
        match VoiceFlow::new(&tx) {
            Ok(flow) => drop(flow),
            // A test machine without any network route: nothing to check.
            Err(e) if e.contains("1231") || e.contains("1168") => eprintln!("no route: {e}"),
            Err(e) => panic!("voice flow: {e}"),
        }
    }
}
