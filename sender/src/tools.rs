//! Helper executables and capture views.
//!
//! DXGI decides which adapter owns each output from the *process's* GPU preference (hybrid
//! laptops). So we make hardlinks of our own exe and of ffmpeg.exe next to tunnelrez-send.exe,
//! give each a fixed preference, and run probes / ffmpeg through them:
//!
//! | link | preference | used for |
//! |---|---|---|
//! | `tunnelrez-probe-igpu.exe`, `ffmpeg-igpu.exe` | power saving | capture on the iGPU |
//! | `tunnelrez-probe-dgpu.exe`, `ffmpeg-dgpu.exe` | high performance | capture on the dGPU |

use crate::encoder::{Capture, GpuPref};
use crate::monitors::{self, Monitor};
use crate::winutil;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

#[derive(Debug)]
pub struct Tools {
    /// ffmpeg.exe, when found (only the ffmpeg backend needs it).
    pub ffmpeg_src: Option<PathBuf>,
    probe: [PathBuf; 2],
    ffmpeg: [Option<PathBuf>; 2],
    /// Per ffmpeg encoder name (`h264_nvenc`, `hevc_nvenc`, `av1_nvenc`): it takes `-cbr_padding`.
    /// Probed once per encoder, on first use.
    padding: Mutex<HashMap<String, bool>>,
}

/// `ffmpeg -h encoder=...` text lists option `name` (lines like `  -cbr_padding  <boolean> ...`).
fn help_has_option(help: &str, name: &str) -> bool {
    help.lines().any(|l| l.trim_start().strip_prefix('-').and_then(|r| r.split_whitespace().next()) == Some(name))
}

/// `ffmpeg` arguments that print `encoder`'s options.
fn help_args(encoder: &str) -> [String; 3] {
    ["-hide_banner".into(), "-h".into(), format!("encoder={encoder}")]
}

/// `probe()` once per `key`; later calls return the stored answer.
fn cached_probe(cache: &Mutex<HashMap<String, bool>>, key: &str, probe: impl FnOnce() -> bool) -> bool {
    if let Some(v) = cache.lock().expect("probe cache lock poisoned").get(key) {
        return *v;
    }
    let v = probe();
    cache.lock().expect("probe cache lock poisoned").insert(key.to_string(), v);
    v
}

fn idx(p: GpuPref) -> usize {
    match p {
        GpuPref::PowerSaving => 0,
        GpuPref::HighPerformance => 1,
    }
}

impl Tools {
    /// Make the GPU-preference hardlinks of this exe (and of ffmpeg.exe, when found).
    ///
    /// # Errors
    ///
    /// When the exe path is unknown, or a hardlink or its GPU preference cannot be written.
    pub fn setup(ffmpeg: Option<&Path>) -> Result<Tools> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().context("exe path has no parent directory")?.to_path_buf();
        let src = ffmpeg.map(Path::to_path_buf).or_else(|| winutil::which("ffmpeg.exe"));
        let mut t = Tools {
            ffmpeg_src: src.clone(),
            probe: Default::default(),
            ffmpeg: Default::default(),
            padding: Mutex::new(HashMap::new()),
        };
        for (pref, val) in [
            (GpuPref::PowerSaving, winutil::GPU_PREF_POWER_SAVING),
            (GpuPref::HighPerformance, winutil::GPU_PREF_HIGH_PERFORMANCE),
        ] {
            let probe = dir.join(format!("tunnelrez-probe-{}.exe", pref.name()));
            winutil::ensure_link(&exe, &probe).with_context(|| format!("link {}", probe.display()))?;
            winutil::set_gpu_preference(&probe, val)?;
            t.probe[idx(pref)] = probe;
            if let Some(src) = &src {
                let ff = dir.join(format!("ffmpeg-{}.exe", pref.name()));
                winutil::ensure_link(src, &ff).with_context(|| format!("link {}", ff.display()))?;
                winutil::set_gpu_preference(&ff, val)?;
                t.ffmpeg[idx(pref)] = Some(ff);
            }
        }
        Ok(t)
    }

    /// `ffmpeg-<p>.exe`.
    ///
    /// # Errors
    /// When ffmpeg.exe was not found.
    pub fn ffmpeg(&self, p: GpuPref) -> Result<&Path> {
        self.ffmpeg[idx(p)].as_deref().ok_or_else(|| anyhow!("ffmpeg.exe not on PATH (use --ffmpeg PATH)"))
    }

    /// This ffmpeg's `encoder` (the one for the requested codec, e.g. `h264_nvenc`) can pad CBR
    /// (`-cbr_padding`, ffmpeg 8+). False when ffmpeg is missing, older (ffmpeg 7.x), or built without that
    /// encoder: the fixed rate then falls back to a ceiling instead of failing to start.
    ///
    /// # Panics
    ///
    /// When the probe cache lock was poisoned.
    pub fn encoder_pads(&self, encoder: &str) -> bool {
        cached_probe(&self.padding, encoder, || {
            let Some(ff) = &self.ffmpeg_src else { return false };
            let mut cmd = Command::new(ff);
            cmd.args(help_args(encoder)).creation_flags(0x0800_0000);
            cmd.output().is_ok_and(|o| help_has_option(&String::from_utf8_lossy(&o.stdout), "cbr_padding"))
        })
    }
    /// Monitors as a process with preference `p` (= `ffmpeg-<p>.exe`) enumerates them.
    ///
    /// # Errors
    ///
    /// When the probe helper cannot run or finds no monitors.
    pub fn monitors(&self, p: GpuPref) -> Result<Vec<Monitor>> {
        let out = Command::new(&self.probe[idx(p)]).arg("--probe-dxgi").output().context("run probe helper")?;
        let v: Vec<Monitor> = String::from_utf8_lossy(&out.stdout).lines().filter_map(monitors::from_line).collect();
        if v.is_empty() {
            bail!("no monitors found by probe ({}): {}", p.name(), String::from_utf8_lossy(&out.stderr));
        }
        Ok(v)
    }

    /// Both views. The power-saving one defines `--monitor N` indices.
    ///
    /// # Errors
    ///
    /// When the power-saving probe fails (the high-performance one may fail: empty view).
    pub fn views(&self) -> Result<Views> {
        Ok(Views {
            igpu: self.monitors(GpuPref::PowerSaving)?,
            dgpu: self.monitors(GpuPref::HighPerformance).unwrap_or_default(),
        })
    }
}

/// What kind of display a monitor is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    Physical,
    /// An indirect display (the Parsec TV monitor).
    Virtual,
}

#[derive(Debug)]
pub struct Views {
    pub igpu: Vec<Monitor>,
    pub dgpu: Vec<Monitor>,
}

impl Views {
    /// Capture options for a monitor (by device name) whose duplication probe succeeded,
    /// power-saving view first, one per adapter vendor.
    ///
    /// ddagrab opens its D3D11 device on adapter 0 of the ffmpeg process and takes `output_idx`
    /// on *that* adapter. So a view only counts when the monitor sits on adapter 0 there. Example:
    /// a Parsec virtual monitor rendered by the NVIDIA GPU is output 0 of adapter 1 (NVIDIA) in the
    /// power-saving view, and output 0 of adapter 0 (Intel) is the laptop panel: ffmpeg-igpu would
    /// capture the wrong screen. The high-performance view (NVIDIA = adapter 0) is the right one.
    pub fn captures(&self, device_name: &str) -> Vec<Capture> {
        captures_in(&[(GpuPref::PowerSaving, &self.igpu), (GpuPref::HighPerformance, &self.dgpu)], device_name)
    }

    /// The monitor under the adapter that renders it.
    pub fn render_adapter(&self, device_name: &str, kind: Display) -> Option<&Monitor> {
        render_adapter_in(&self.igpu, &self.dgpu, device_name, kind)
    }
}

/// The power-saving view is right unless DXGI moved the output there: with a laptop in dedicated-GPU mode it lists
/// outputs of the NVIDIA GPU under the integrated GPU too (measured: the panel is under Intel, where duplication says
/// `DXGI_ERROR_UNSUPPORTED`, and under NVIDIA in the high-performance view). An indirect display that really renders
/// on the integrated GPU stays under it in both views (measured with a `DisplayLink` monitor), so one that the two
/// views disagree on is on the high-performance GPU, provided it can really be duplicated there: on a hybrid laptop
/// a monitor of the integrated GPU can be listed under NVIDIA and fail with `DXGI_ERROR_ACCESS_LOST`.
fn render_adapter_in<'a>(
    power_saving: &'a [Monitor],
    high_performance: &'a [Monitor],
    device_name: &str,
    kind: Display,
) -> Option<&'a Monitor> {
    let find = |view: &'a [Monitor]| view.iter().find(|m| m.device_name == device_name);
    match (find(power_saving), find(high_performance)) {
        (Some(p), Some(h))
            if p.vendor_id != h.vendor_id && h.dup.is_ok() && (kind == Display::Virtual || p.dup.is_err()) =>
        {
            Some(h)
        }
        (Some(p), _) => Some(p),
        (None, h) => h,
    }
}

fn captures_in(views: &[(GpuPref, &Vec<Monitor>)], device_name: &str) -> Vec<Capture> {
    let mut v: Vec<Capture> = Vec::new();
    for &(pref, list) in views {
        let usable = list.iter().filter(|m| m.device_name == device_name && m.dup.is_ok() && m.adapter_idx == 0);
        for m in usable {
            if v.iter().any(|c| c.vendor == m.vendor_id) {
                continue;
            }
            v.push(Capture {
                pref,
                output_idx: m.output_idx,
                vendor: m.vendor_id,
                adapter: m.adapter_name.clone(),
                width: u32::try_from(m.width).unwrap_or(0),
                height: u32::try_from(m.height).unwrap_or(0),
            });
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitors::{VENDOR_INTEL, VENDOR_NVIDIA};

    fn mon(adapter_idx: u32, vendor_id: u32, output_idx: u32, name: &str) -> Monitor {
        Monitor {
            index: 0,
            adapter_idx,
            adapter_name: format!("gpu{adapter_idx}"),
            vendor_id,
            luid: u64::from(adapter_idx),
            output_idx,
            device_name: name.into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            dup: Ok(()),
        }
    }

    /// Each case below was read from a real laptop (`--probe-dxgi` in both views, or the sender's logs).
    #[test]
    fn render_adapter_is_not_fooled_by_the_power_saving_view() {
        let nvidia = |m: Option<&Monitor>| m.map(|m| m.vendor_id) == Some(VENDOR_NVIDIA);
        let intel = |m: Option<&Monitor>| m.map(|m| m.vendor_id) == Some(VENDOR_INTEL);
        // Dedicated-GPU mode. The panel: moved under Intel in the power-saving view, not duplicable there.
        let mut panel_ps = mon(0, VENDOR_INTEL, 0, "PANEL");
        panel_ps.dup = Err("DXGI_ERROR_UNSUPPORTED".into());
        let igpu = vec![panel_ps, mon(0, VENDOR_INTEL, 1, "LINK"), mon(0, VENDOR_INTEL, 2, "VIRT")];
        let dgpu =
            vec![mon(0, VENDOR_NVIDIA, 0, "PANEL"), mon(1, VENDOR_INTEL, 0, "LINK"), mon(0, VENDOR_NVIDIA, 1, "VIRT")];
        assert!(nvidia(render_adapter_in(&igpu, &dgpu, "PANEL", Display::Physical)));
        // The TV monitor: the two views disagree, so it is not on Intel.
        assert!(nvidia(render_adapter_in(&igpu, &dgpu, "VIRT", Display::Virtual)));
        // The DisplayLink monitor really renders on Intel: both views say so.
        assert!(intel(render_adapter_in(&igpu, &dgpu, "LINK", Display::Virtual)));
        // Hybrid mode: the panel is on Intel and duplicable there; the high-performance view moves it under NVIDIA.
        let igpu = vec![mon(0, VENDOR_INTEL, 0, "PANEL"), mon(1, VENDOR_NVIDIA, 0, "VIRT")];
        let dgpu = vec![mon(0, VENDOR_NVIDIA, 0, "PANEL"), mon(0, VENDOR_NVIDIA, 1, "VIRT")];
        assert!(intel(render_adapter_in(&igpu, &dgpu, "PANEL", Display::Physical)));
        assert!(nvidia(render_adapter_in(&igpu, &dgpu, "VIRT", Display::Virtual)));
        // Listed under NVIDIA, but duplication fails there: that view proves nothing.
        let mut failed = mon(0, VENDOR_NVIDIA, 1, "VIRT");
        failed.dup = Err("DXGI_ERROR_ACCESS_LOST on first frame".into());
        let dgpu_failed = vec![mon(0, VENDOR_NVIDIA, 0, "PANEL"), failed];
        let igpu_intel = vec![mon(0, VENDOR_INTEL, 0, "PANEL"), mon(0, VENDOR_INTEL, 1, "VIRT")];
        assert!(intel(render_adapter_in(&igpu_intel, &dgpu_failed, "VIRT", Display::Virtual)));
        // Only one view lists it, or none does.
        assert!(nvidia(render_adapter_in(&[], &dgpu, "VIRT", Display::Virtual)));
        assert!(render_adapter_in(&igpu, &dgpu, "GONE", Display::Virtual).is_none());
    }

    #[test]
    fn padding_is_probed_per_encoder() {
        // A custom build with h264_nvenc but no hevc_nvenc: each codec asks about its own encoder.
        let cache = Mutex::new(HashMap::new());
        let mut asked = Vec::new();
        let mut probe = |enc: &str, has: bool| {
            cached_probe(&cache, enc, || {
                asked.push(enc.to_string());
                has
            })
        };
        assert!(probe("h264_nvenc", true));
        assert!(!probe("hevc_nvenc", false));
        assert!(probe("h264_nvenc", false), "cached: not asked again");
        assert_eq!(asked, ["h264_nvenc", "hevc_nvenc"]);
        assert_eq!(help_args("av1_nvenc")[2], "encoder=av1_nvenc");
    }

    #[test]
    fn cbr_padding_option_detection() {
        let ffmpeg8 = "  -cbr               <boolean>    E..V....... Use cbr encoding mode (default false)\n  \
                       -cbr_padding       <boolean>    E..V....... Pad the bitstream (default false)\n";
        let ffmpeg71 = "  -cbr               <boolean>    E..V....... Use cbr encoding mode (default false)\n  \
                        -qmin              <int>        E..V....... min QP\n";
        assert!(help_has_option(ffmpeg8, "cbr_padding"));
        assert!(!help_has_option(ffmpeg71, "cbr_padding"), "FFmpeg 7.1 has no cbr_padding");
        assert!(help_has_option(ffmpeg71, "cbr"), "exact name, not a prefix");
    }

    #[test]
    fn virtual_monitor_on_nvidia_uses_the_high_performance_view() {
        // Power-saving view: Intel = adapter 0 (laptop panel), NVIDIA = adapter 1 (virtual monitor).
        let igpu = vec![mon(0, VENDOR_INTEL, 0, "PANEL"), mon(1, VENDOR_NVIDIA, 0, "VIRT")];
        // High-performance view: NVIDIA = adapter 0, owns both outputs.
        let dgpu = vec![mon(0, VENDOR_NVIDIA, 0, "PANEL"), mon(0, VENDOR_NVIDIA, 1, "VIRT")];
        let views = [(GpuPref::PowerSaving, &igpu), (GpuPref::HighPerformance, &dgpu)];
        let c = captures_in(&views, "VIRT");
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].pref, c[0].vendor, c[0].output_idx), (GpuPref::HighPerformance, VENDOR_NVIDIA, 1));
        // The laptop panel stays on the Intel power-saving view first.
        let c = captures_in(&views, "PANEL");
        assert_eq!((c[0].pref, c[0].vendor, c[0].output_idx), (GpuPref::PowerSaving, VENDOR_INTEL, 0));
    }
}
