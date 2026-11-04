//! Small stats helpers and the PING/PONG clock-offset estimator.

use crate::num::{f32_from_usize, usize_sat_from_f32};
use std::collections::VecDeque;

/// (p50, p99, max) of a sample set; zeros when empty.
pub fn pct(v: &mut [f32]) -> (f32, f32, f32) {
    v.sort_by(f32::total_cmp);
    let Some(&max) = v.last() else {
        return (0.0, 0.0, 0.0);
    };
    let last = v.len() - 1;
    let at = |q: f32| v[usize_sat_from_f32((f32_from_usize(last) * q).round()).min(last)];
    (at(0.5), at(0.99), max)
}

pub fn r1(x: f32) -> f64 {
    (f64::from(x) * 10.0).round() / 10.0
}

/// Clock offset estimate from PING/PONG: offset = `receiver_clock` - `sender_clock`.
/// Uses the minimum-RTT sample of a sliding window (NTP-style).
#[derive(Default, Debug)]
pub struct ClockEst {
    samples: VecDeque<(u64, u64, i64)>, // (sender recv time, rtt_us, offset_us)
}

impl ClockEst {
    /// `t0` = our send time echoed back, `tr` = receiver time, `t3` = our receive time.
    pub fn add(&mut self, t0: u64, tr: u64, t3: u64) {
        if t3 < t0 {
            return;
        }
        let rtt = t3 - t0;
        // Two's-complement difference, same as the old `as i64` subtraction.
        let off = tr.wrapping_sub(t0 + rtt / 2).cast_signed();
        self.samples.push_back((t3, rtt, off));
        while self.samples.len() > 40 {
            self.samples.pop_front(); // 40 x 500 ms = 20 s window
        }
    }
    /// (`rtt_us`, `offset_us`) of the min-RTT sample.
    pub fn best(&self) -> Option<(u64, i64)> {
        self.samples.iter().min_by_key(|s| s.1).map(|s| (s.1, s.2))
    }
    pub fn last_rtt(&self) -> Option<u64> {
        self.samples.back().map(|s| s.1)
    }
}
