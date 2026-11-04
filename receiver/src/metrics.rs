//! Small numeric helpers for the 1 Hz STATS line.

/// Microseconds to milliseconds, for stats samples.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "stats only: sample spans are far below 2^24 us, where f32 is exact enough"
)]
pub(crate) fn us_to_ms(us: u64) -> f32 {
    us as f32 / 1000.0
}

/// A count as f32 (counts per second in STATS are small).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "stats only: small counts")]
pub(crate) fn count_f32(n: u64) -> f32 {
    n as f32
}

/// p50 / p99 of a sample set (sorts in place). (0, 0) for empty input.
pub(crate) fn percentiles(v: &mut [f32]) -> (f32, f32) {
    v.sort_by(f32::total_cmp);
    (quantile(v, 50), quantile(v, 99))
}

/// The `pct`-th percentile of sorted samples (nearest rank, rounded); 0 for empty input.
fn quantile(sorted: &[f32], pct: usize) -> f32 {
    let Some(last) = sorted.len().checked_sub(1) else {
        return 0.0;
    };
    // Round (last * pct / 100) to the nearest index.
    let idx = (last * pct + 50) / 100;
    sorted[idx.min(last)]
}

/// Largest sample of a sorted set; 0 for empty input.
pub(crate) fn max_sorted(sorted: &[f32]) -> f32 {
    sorted.last().copied().unwrap_or(0.0)
}

/// Round to one decimal for JSON.
pub(crate) fn round1(x: f32) -> f64 {
    (f64::from(x) * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_known_set() {
        let mut v: Vec<f32> = (1..=100).rev().map(count_f32).collect();
        let (p50, p99) = percentiles(&mut v);
        assert!((p50 - 51.0).abs() < 0.01, "p50 {p50}");
        assert!((p99 - 99.0).abs() < 0.01, "p99 {p99}");
        assert!((max_sorted(&v) - 100.0).abs() < 0.01);
        assert_eq!(percentiles(&mut []), (0.0, 0.0));
    }

    #[test]
    fn single_sample() {
        assert_eq!(percentiles(&mut [7.0]), (7.0, 7.0));
    }

    #[test]
    fn conversions() {
        assert!((us_to_ms(16_667) - 16.667).abs() < 0.001);
        assert!((round1(1.26) - 1.3).abs() < 1e-9);
    }
}
