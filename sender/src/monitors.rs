//! DXGI adapter/output enumeration and a Desktop Duplication probe.
//!
//! On a hybrid laptop the adapter order, and which adapter "owns" each output, depend on the
//! GPU preference of the *process*. On this PC the default is high-performance: every output
//! (internal panel, `DisplayLink`, the Parsec virtual display) shows up under the NVIDIA adapter,
//! but duplication there fails (internal panel: `DXGI_ERROR_UNSUPPORTED`; IddCx/DisplayLink:
//! `DXGI_ERROR_ACCESS_LOST` on the first frame). In a power-saving process the Intel adapter owns
//! all outputs and duplication works. ffmpeg's ddagrab picks `output_idx` on the adapter of its
//! D3D11 device, so the indices we hand to ffmpeg come from a probe process with the same GPU
//! preference as that ffmpeg (we probe both a power-saving and a high-performance view).

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, DXGI_OUTDUPL_FRAME_INFO,
};

#[derive(Debug, Clone)]
pub struct Monitor {
    /// Global index (enumeration order) used by `--monitor N`.
    pub index: usize,
    pub adapter_idx: u32,
    pub adapter_name: String,
    pub vendor_id: u32,
    /// Adapter LUID (`HighPart << 32 | LowPart`): the adapter that renders this output in this view.
    pub luid: u64,
    pub output_idx: u32,
    pub device_name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Result of `IDXGIOutput1::DuplicateOutput` with a device on the owning adapter.
    pub dup: Result<(), String>,
}

fn wstr(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

pub const VENDOR_NVIDIA: u32 = 0x10de;
pub const VENDOR_INTEL: u32 = 0x8086;
pub const VENDOR_AMD: u32 = 0x1002;

pub fn vendor_name(id: u32) -> &'static str {
    match id {
        VENDOR_NVIDIA => "NVIDIA",
        VENDOR_INTEL => "Intel",
        VENDOR_AMD => "AMD",
        0x1414 => "Microsoft",
        _ => "other",
    }
}

const DXGI_ERROR_UNSUPPORTED: u32 = 0x887a_0004;
const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: u32 = 0x887a_0022;
const DXGI_ERROR_ACCESS_LOST: u32 = 0x887a_0026;
const DXGI_ERROR_WAIT_TIMEOUT: u32 = 0x887a_0027;

/// HRESULT bits as `u32`, for hex printing and matching DXGI error codes.
fn hr(e: &windows::core::Error) -> u32 {
    e.code().0.cast_unsigned()
}

/// Whether [`list`] also tries Desktop Duplication on every output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DupProbe {
    /// Try `DuplicateOutput` plus one frame per output (fills [`Monitor::dup`]).
    Probe,
    /// Skip it (`dup` is always `Ok`).
    Skip,
}

fn try_duplicate(adapter: &IDXGIAdapter1, output: &IDXGIOutput) -> Result<(), String> {
    let mut dev: Option<ID3D11Device> = None;
    // SAFETY: `adapter` is a live COM interface borrowed for the call; `dev` is a live local
    // out-param; no feature-level array, context or feature-level out-params are passed.
    let created = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&raw mut dev),
            None,
            None,
        )
    };
    created.map_err(|e| format!("D3D11CreateDevice 0x{:08x}", hr(&e)))?;
    let dev = dev.ok_or("no device")?;
    let out1: IDXGIOutput1 = output.cast().map_err(|e| format!("IDXGIOutput1: {e}"))?;
    // SAFETY: `out1` and `dev` are live COM interfaces owned by this function.
    let dup = unsafe { out1.DuplicateOutput(&dev) }.map_err(|e| {
        let c = hr(&e);
        match c {
            DXGI_ERROR_UNSUPPORTED => "DXGI_ERROR_UNSUPPORTED".to_string(),
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE".to_string(),
            _ => format!("0x{c:08x}"),
        }
    })?;
    // DuplicateOutput can succeed on a view where frames never come (hybrid laptop: the
    // dGPU view of an iGPU-owned output fails with DXGI_ERROR_ACCESS_LOST on the first
    // AcquireNextFrame). Try one frame; a timeout (static desktop) still counts as working.
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut res = None;
    // SAFETY: `dup` is a live duplication interface; `info` and `res` are live local out-params.
    let acquired = unsafe { dup.AcquireNextFrame(50, &raw mut info, &raw mut res) };
    match acquired {
        Ok(()) => {
            // SAFETY: a frame was acquired on `dup` just above; it is released once, before
            // `dup` is dropped.
            let _ = unsafe { dup.ReleaseFrame() };
        }
        Err(e) if hr(&e) == DXGI_ERROR_WAIT_TIMEOUT => {}
        Err(e) => {
            let c = hr(&e);
            return Err(match c {
                DXGI_ERROR_ACCESS_LOST => "DXGI_ERROR_ACCESS_LOST on first frame".to_string(),
                _ => format!("AcquireNextFrame 0x{c:08x}"),
            });
        }
    }
    drop(dup);
    Ok(())
}

/// Enumerate outputs in plain `EnumAdapters1` order (what ffmpeg's d3d11va device index uses).
///
/// # Errors
///
/// When the DXGI factory cannot be created or an adapter/output descriptor cannot be read.
pub fn list(probe: DupProbe) -> anyhow::Result<Vec<Monitor>> {
    let mut v = Vec::new();
    // SAFETY: plain factory creation, no arguments.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut a = 0u32;
    // SAFETY: `factory` is a live COM interface; enumerating past the end returns an error.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        // SAFETY: `adapter` is a live COM interface; the descriptor is returned by value.
        let desc = unsafe { adapter.GetDesc1() }?;
        let mut o = 0u32;
        // SAFETY: `adapter` is a live COM interface; enumerating past the end returns an error.
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            // SAFETY: `output` is a live COM interface; the descriptor is returned by value.
            let od = unsafe { output.GetDesc() }?;
            let r = od.DesktopCoordinates;
            let dup = match probe {
                DupProbe::Probe => try_duplicate(&adapter, &output),
                DupProbe::Skip => Ok(()),
            };
            v.push(Monitor {
                index: v.len(),
                adapter_idx: a,
                adapter_name: wstr(&desc.Description),
                vendor_id: desc.VendorId,
                luid: (u64::from(desc.AdapterLuid.HighPart.cast_unsigned()) << 32)
                    | u64::from(desc.AdapterLuid.LowPart),
                output_idx: o,
                device_name: wstr(&od.DeviceName),
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                dup,
            });
            o += 1;
        }
        a += 1;
    }
    Ok(v)
}

/// One tab-separated line per monitor, printed by the `--probe-dxgi` child.
pub fn to_line(m: &Monitor) -> String {
    format!(
        "MON\t{}\t{}\t{}\t{}\t{:x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        m.index,
        m.adapter_idx,
        m.adapter_name,
        m.vendor_id,
        m.luid,
        m.output_idx,
        m.device_name,
        m.x,
        m.y,
        m.width,
        m.height,
        match &m.dup {
            Ok(()) => "ok".to_string(),
            Err(e) => e.clone(),
        }
    )
}

pub fn from_line(l: &str) -> Option<Monitor> {
    let f: Vec<&str> = l.trim_end().split('\t').collect();
    if f.len() != 13 || f[0] != "MON" {
        return None;
    }
    Some(Monitor {
        index: f[1].parse().ok()?,
        adapter_idx: f[2].parse().ok()?,
        adapter_name: f[3].to_string(),
        vendor_id: f[4].parse().ok()?,
        luid: u64::from_str_radix(f[5], 16).ok()?,
        output_idx: f[6].parse().ok()?,
        device_name: f[7].to_string(),
        x: f[8].parse().ok()?,
        y: f[9].parse().ok()?,
        width: f[10].parse().ok()?,
        height: f[11].parse().ok()?,
        dup: if f[12] == "ok" { Ok(()) } else { Err(f[12].to_string()) },
    })
}
