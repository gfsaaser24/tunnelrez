//! Per-app GPU preference (`HKCU\Software\Microsoft\DirectX\UserGpuPreferences`), as set by
//! Settings > System > Display > Graphics. On a hybrid laptop it decides which adapter owns the outputs in
//! a process's DXGI view, which decides where Desktop Duplication works.

use crate::conv;
use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::path::Path;
use windows::core::HSTRING;
use windows::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};

/// `GpuPreference=0`: let Windows decide.
pub const UNSPECIFIED: u32 = 0;
/// `GpuPreference=1`: power saving (the iGPU on a hybrid laptop).
pub const POWER_SAVING: u32 = 1;
/// `GpuPreference=2`: high performance (the dGPU on a hybrid laptop).
pub const HIGH_PERFORMANCE: u32 = 2;

const KEY: &str = "Software\\Microsoft\\DirectX\\UserGpuPreferences";

/// Store `pref` (one of the constants above) as the GPU preference of `exe`.
///
/// # Errors
///
/// The registry write failed.
pub fn set(exe: &Path, pref: u32) -> Result<()> {
    let val: Vec<u16> = format!("GpuPreference={pref};").encode_utf16().chain(Some(0)).collect();
    let bytes = u32::try_from(std::mem::size_of_val(val.as_slice())).context("GPU preference value too long")?;
    // SAFETY: `val` is a NUL-terminated UTF-16 buffer alive for the call; the size passed is its byte length.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(KEY),
            &HSTRING::from(exe.as_os_str()),
            REG_SZ.0,
            Some(val.as_ptr().cast::<c_void>()),
            bytes,
        )
    };
    if r.is_err() {
        bail!("RegSetKeyValueW: {r:?}");
    }
    Ok(())
}

/// The preference stored for `exe` (None if not set).
pub fn get(exe: &Path) -> Option<u32> {
    let mut buf = [0u16; 128];
    let mut len = conv::size_of_u32::<[u16; 128]>();
    // SAFETY: `buf` and `len` describe one writable buffer that outlives the call; RegGetValueW writes at most `len` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(KEY),
            &HSTRING::from(exe.as_os_str()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&raw mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    let s = crate::d3d::wstr(&buf);
    s.split(';').find_map(|kv| kv.strip_prefix("GpuPreference=")).and_then(|v| v.trim().parse().ok())
}
