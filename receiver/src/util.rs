//! Small Android helpers: monotonic clock, system properties, local IP addresses, thread priority.

use std::ffi::{CStr, CString};

fn monotonic() -> libc::timespec {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    ts
}

/// `CLOCK_MONOTONIC` in microseconds (same clock as `System.nanoTime` / AMediaCodec render timestamps).
pub(crate) fn now_us() -> u64 {
    let ts = monotonic();
    let s = u64::try_from(ts.tv_sec).unwrap_or(0);
    let ns = u64::try_from(ts.tv_nsec).unwrap_or(0);
    s * 1_000_000 + ns / 1_000
}

/// `CLOCK_MONOTONIC` in nanoseconds.
#[cfg_attr(
    target_pointer_width = "64",
    expect(clippy::useless_conversion, reason = "time_t and long are i32 on 32-bit Android, i64 on 64-bit")
)]
pub(crate) fn now_ns() -> i64 {
    let ts = monotonic();
    i64::from(ts.tv_sec) * 1_000_000_000 + i64::from(ts.tv_nsec)
}

/// Read an Android system property (e.g. `debug.lrt.render`). Empty string if unset.
pub(crate) fn get_prop(name: &str) -> String {
    let Ok(cname) = CString::new(name) else {
        return String::new();
    };
    let mut buf: [libc::c_char; 92] = [0; 92]; // PROP_VALUE_MAX
                                               // SAFETY: `cname` is NUL-terminated; `buf` has PROP_VALUE_MAX bytes, the most the call writes.
    let n = unsafe { libc::__system_property_get(cname.as_ptr(), buf.as_mut_ptr()) };
    if n <= 0 {
        return String::new();
    }
    // SAFETY: the call wrote a NUL-terminated string into `buf`.
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

/// A numeric property, or `default` if unset or not a number.
pub(crate) fn get_prop_i32(name: &str, default: i32) -> i32 {
    get_prop(name).trim().parse().unwrap_or(default)
}

/// Non-loopback IPv4 addresses with their interface names, wlan/eth first.
pub(crate) fn local_ipv4() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `ifap` is a valid out-pointer; the list is freed below with freeifaddrs.
    if unsafe { libc::getifaddrs(&raw mut ifap) } != 0 {
        return out;
    }
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: `cur` is a non-null node of the list getifaddrs returned, alive until freeifaddrs.
        let ifa = unsafe { &*cur };
        if let Some(entry) = ipv4_of(ifa) {
            out.push(entry);
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: `ifap` came from getifaddrs and is freed once; no reference into it outlives this point.
    unsafe { libc::freeifaddrs(ifap) };
    out.sort_by_key(|(n, _)| u8::from(!(n.starts_with("wlan") || n.starts_with("eth"))));
    out
}

/// (interface name, address) of one ifaddrs node if it is a non-loopback IPv4 address.
fn ipv4_of(ifa: &libc::ifaddrs) -> Option<(String, String)> {
    if ifa.ifa_addr.is_null() {
        return None;
    }
    // SAFETY: `ifa_addr` is non-null and points to a sockaddr owned by the ifaddrs list.
    let family = unsafe { (*ifa.ifa_addr).sa_family };
    if i32::from(family) != libc::AF_INET {
        return None;
    }
    // SAFETY: family is AF_INET, so the sockaddr is a sockaddr_in.
    let sin = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in>() };
    let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
    if ip.is_loopback() {
        return None;
    }
    // SAFETY: `ifa_name` is a NUL-terminated string owned by the ifaddrs list.
    let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
    Some((name, ip.to_string()))
}

/// Best single IP string for display.
pub(crate) fn primary_ip() -> String {
    local_ipv4().into_iter().next().map_or_else(|| "0.0.0.0".into(), |(_, ip)| ip)
}

/// Set the calling thread's nice value (negative = higher priority). Best effort.
pub(crate) fn set_thread_priority(nice: i32) {
    // SAFETY: gettid has no preconditions.
    let tid = unsafe { libc::gettid() };
    let Ok(tid) = libc::id_t::try_from(tid) else {
        return;
    };
    // SAFETY: plain syscall on our own thread id; failure only leaves the priority unchanged.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
}
