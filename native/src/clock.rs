//! Sender clock: `QueryPerformanceCounter` in microseconds. Same clock as `sender/src/clock.rs`
//! (`now_us`), which is the protocol's `t_us`. Duplicated here so this crate has no sender dependency.

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

fn freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: QueryPerformanceFrequency only writes the i64 we pass; it cannot fail on XP and later.
        unsafe { QueryPerformanceFrequency(&raw mut f) }
            .expect("QueryPerformanceFrequency cannot fail on Windows XP and later");
        f
    })
}

/// Monotonic microseconds (QPC based). The protocol's sender clock (`t_us`).
///
/// # Panics
///
/// If `QueryPerformanceCounter` fails, which Windows documents as impossible on XP and later.
pub fn now_us() -> u64 {
    let mut c = 0i64;
    // SAFETY: QueryPerformanceCounter only writes the i64 we pass.
    unsafe { QueryPerformanceCounter(&raw mut c) }
        .expect("QueryPerformanceCounter cannot fail on Windows XP and later");
    qpc_to_us(c)
}

/// Convert a raw QPC value (e.g. `DXGI_OUTDUPL_FRAME_INFO::LastPresentTime`) to `now_us` microseconds.
pub fn qpc_to_us(c: i64) -> u64 {
    // QPC values are never negative; a negative input maps to 0.
    u64::try_from(i128::from(c) * 1_000_000 / i128::from(freq())).unwrap_or_default()
}

/// Convert a `WinRT` `SystemRelativeTime` (QPC time in 100 ns units) to `now_us` microseconds.
pub fn hns_to_us(hns: i64) -> u64 {
    // System-relative times are never negative; a negative input maps to 0.
    u64::try_from(hns / 10).unwrap_or_default()
}
