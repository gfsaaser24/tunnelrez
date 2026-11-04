//! The few numeric `as` casts this crate wants on purpose, each in one tiny helper so the
//! lint expectation sits on the narrowest item and every call site reads as intent.
//!
//! Lossless conversions use `From` / `into()` at the call site, and fallible ones use
//! `try_from`; only the deliberately lossy ones (float stats math, saturating float to
//! integer) live here.

/// `u64` -> `f32` for stats, rates and log output (rounding above 2^24 is fine there).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "stats/log math: rounding is intended")]
pub fn f32_from_u64(x: u64) -> f32 {
    x as f32
}

/// `u128` -> `f32` (e.g. `Duration::as_millis`) for stats and log output.
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "stats/log math: rounding is intended")]
pub fn f32_from_u128(x: u128) -> f32 {
    x as f32
}

/// `u32` -> `f32` for stats and log output.
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "stats/log math: rounding is intended")]
pub fn f32_from_u32(x: u32) -> f32 {
    x as f32
}

/// `usize` -> `f32` for stats (counts and indices far below 2^24).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "stats math: rounding is intended")]
pub fn f32_from_usize(x: usize) -> f32 {
    x as f32
}

/// `u64` -> `f64` for rate math and log output (exact below 2^53).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "rate/log math: rounding is intended")]
pub fn f64_from_u64(x: u64) -> f64 {
    x as f64
}

/// `i64` -> `f64` for clock-offset math (exact below 2^53).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "clock math: rounding is intended")]
pub fn f64_from_i64(x: i64) -> f64 {
    x as f64
}

/// `i64` -> `f32` for log output.
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "log math: rounding is intended")]
pub fn f32_from_i64(x: i64) -> f32 {
    x as f32
}

/// `f64` -> `f32` for stats and log output.
#[expect(clippy::as_conversions, clippy::cast_possible_truncation, reason = "stats/log math: rounding is intended")]
pub fn f32_from_f64(x: f64) -> f32 {
    x as f32
}

/// `f64` -> `u64`, saturating: negative and NaN give 0, too large gives `u64::MAX`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "saturating float-to-int is the intent (`as` saturates)"
)]
pub fn u64_sat_from_f64(x: f64) -> u64 {
    x as u64
}

/// `f32` -> `u64`, saturating: negative and NaN give 0, too large gives `u64::MAX`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "saturating float-to-int is the intent (`as` saturates)"
)]
pub fn u64_sat_from_f32(x: f32) -> u64 {
    x as u64
}

/// `f32` -> `usize`, saturating: negative and NaN give 0, too large gives `usize::MAX`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "saturating float-to-int is the intent (`as` saturates)"
)]
pub fn usize_sat_from_f32(x: f32) -> usize {
    x as usize
}

/// `i32` -> `f32` for PCM sample math (24/32-bit samples lose low bits, inaudible).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "PCM math: rounding is intended")]
pub fn f32_from_i32(x: i32) -> f32 {
    x as f32
}

/// `f32` -> `i16`, rounded and saturating (NaN gives 0): float PCM to 16-bit PCM.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "saturating float-to-int is the intent (`as` saturates)"
)]
pub fn i16_sat_from_f32(x: f32) -> i16 {
    x.round() as i16
}

/// `f64` -> `i64`, rounded and saturating (NaN gives 0).
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "saturating float-to-int is the intent (`as` saturates)"
)]
pub fn i64_sat_from_f64(x: f64) -> i64 {
    x.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_float_to_int() {
        assert_eq!(u64_sat_from_f64(-1.0), 0);
        assert_eq!(u64_sat_from_f64(f64::NAN), 0);
        assert_eq!(u64_sat_from_f64(1e30), u64::MAX);
        assert_eq!(u64_sat_from_f64(2.9), 2);
        assert_eq!(usize_sat_from_f32(3.5), 3);
        assert_eq!(u64_sat_from_f32(-0.5), 0);
        assert_eq!(i16_sat_from_f32(40_000.0), i16::MAX);
        assert_eq!(i16_sat_from_f32(-1.6), -2);
        assert_eq!(i16_sat_from_f32(f32::NAN), 0);
        assert_eq!(i64_sat_from_f64(-2.5), -3);
    }
}
