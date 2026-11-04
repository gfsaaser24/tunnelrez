//! Sender clock: `QueryPerformanceCounter` in microseconds (system-wide monotonic, same clock
//! for every process on this PC). Plus a mapping from Windows wall-clock time (what ffmpeg's
//! `RTCTIME` reads) to that monotonic clock.

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

fn freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: `f` is a live i64 out-param. QPF cannot fail on Windows XP and later.
        unsafe { QueryPerformanceFrequency(&raw mut f) }
            .expect("QueryPerformanceFrequency cannot fail on Windows XP and later");
        f
    })
}

/// Monotonic microseconds (QPC based). This is the protocol's sender clock (`t_us`).
///
/// # Panics
///
/// Never in practice: QPC/QPF cannot fail on Windows XP and later, and the counter is
/// non-negative.
pub fn now_us() -> u64 {
    let mut c = 0i64;
    // SAFETY: `c` is a live i64 out-param. QPC cannot fail on Windows XP and later.
    unsafe { QueryPerformanceCounter(&raw mut c) }
        .expect("QueryPerformanceCounter cannot fail on Windows XP and later");
    let f = freq();
    u64::try_from((i128::from(c) * 1_000_000) / i128::from(f)).expect("QPC microseconds are non-negative and fit u64")
}

/// Wall-clock microseconds since the Unix epoch (precise system time).
pub fn unix_us() -> i64 {
    // SAFETY: no arguments; returns a FILETIME by value.
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    let t = (i64::from(ft.dwHighDateTime) << 32) | i64::from(ft.dwLowDateTime);
    (t - 116_444_736_000_000_000) / 10
}

/// A sampled (wall, mono) pair used to convert wall-clock times into the monotonic clock.
#[derive(Clone, Copy, Debug)]
pub struct WallToMono {
    pub unix0: i64,
    pub mono0: u64,
}

impl WallToMono {
    /// Take several tight samples and keep the one with the smallest bracketing interval.
    pub fn sample() -> Self {
        let mut best = (u64::MAX, WallToMono { unix0: 0, mono0: 0 });
        for _ in 0..16 {
            let a = now_us();
            let u = unix_us();
            let b = now_us();
            if b - a < best.0 {
                best = (b - a, WallToMono { unix0: u, mono0: a + (b - a) / 2 });
            }
        }
        best.1
    }
    pub fn mono(&self, unix: i64) -> u64 {
        // Two's-complement wrap, same as the old `as` round trip through i64.
        self.mono0.wrapping_add_signed(unix - self.unix0)
    }
}
