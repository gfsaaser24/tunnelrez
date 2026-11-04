//! Small numeric conversions that `From` cannot express, each checked or justified in one place.

/// `u32` to `usize`. Lossless: `usize` is at least 32 bits on every Windows target this crate builds for.
#[expect(clippy::as_conversions, reason = "usize is 32 or 64 bits on Windows, so u32 always fits")]
pub(crate) const fn usize_from_u32(v: u32) -> usize {
    v as usize
}

/// `size_of::<T>()` as the `u32` that Win32 / NVENC size fields take.
///
/// # Panics
///
/// If `T` is 4 GiB or larger, which no FFI struct is.
pub(crate) const fn size_of_u32<T>() -> u32 {
    let n = std::mem::size_of::<T>();
    assert!(n <= 0xFFFF_FFFF, "FFI struct size does not fit u32");
    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, reason = "checked by the assert above")]
    let n = n as u32;
    n
}

/// `f64` to `u32`, saturating: NaN and negatives give 0, values above `u32::MAX` give `u32::MAX`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float to int `as` saturates, which is the intended rounding toward zero"
)]
pub(crate) const fn u32_from_f64_sat(v: f64) -> u32 {
    v as u32
}
