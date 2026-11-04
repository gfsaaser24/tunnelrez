//! Capture + encode for N seconds and print frame pacing and latency percentiles.
//!
//!   cargo run --release --example bench -- [options]
//!     --seconds N          per run (default 10)
//!     --res 1080|4k|WxH    encode size (default: the output's size; other sizes are GPU-scaled)
//!     --fps N              (default 60)
//!     --codec hevc|h264    (default hevc)
//!     --bitrate MBPS       (default 30 at 1080p, 80 at 4K)
//!     --output virtual|primary|DISPLAYn|INDEX   (default virtual = the Parsec virtual display)
//!     --capture auto|dda|wgc   --gpu auto|encode|other   --copy auto|direct|cross|staging
//!     --animate            draw moving bars on the captured output (forces a new frame every vsync)
//!     --dump FILE          write the elementary stream (check with ffplay / ffprobe)
//!     --matrix             run every capture/copy combination at 1080p and 4K (implies --animate)
//!     --list               list outputs and exit
//!
//! Desktop Duplication needs the power-saving GPU preference on hybrid laptops; bench sets it for its own exe
//! (`prefer_power_saving_gpu`) before touching DXGI.

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tunnelrez_native::{
    list_outputs, prefer_power_saving_gpu, CaptureApi, CaptureGpu, Codec, CopyPath, NativeConfig, NativeSession,
    OutputInfo, OutputSel,
};

#[derive(Clone)]
struct Run {
    name: String,
    cfg: NativeConfig,
}

fn parse_res(s: &str) -> Result<(u32, u32)> {
    Ok(match s {
        "1080" | "1080p" => (1920, 1080),
        "4k" | "4K" | "2160" | "2160p" => (3840, 2160),
        "720" | "720p" => (1280, 720),
        _ => {
            let (w, h) = s.split_once('x').ok_or_else(|| anyhow::anyhow!("bad --res {s}"))?;
            (w.parse()?, h.parse()?)
        }
    })
}

/// Microsecond count to `f64` for the statistics below (precision loss only above 2^53 us).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "intended float statistics")]
fn f64_of(v: u64) -> f64 {
    v as f64
}

/// Index of quantile `q` (0..=1) in a sorted slice of `len` (> 0) items, rounded to nearest.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float quantile math on small sample counts; the rounded value is in 0..len"
)]
fn quantile_index(len: usize, q: f64) -> usize {
    ((len - 1) as f64 * q).round() as usize
}

/// Mbit/s (float) to bit/s, saturating at the `u32` range like an `as` cast.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float to int `as` saturates, which is the intended clamping"
)]
fn bps_from_mbps(mbps: f64) -> u32 {
    (mbps * 1e6) as u32
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("latency samples are never NaN"));
    v[quantile_index(v.len(), q)]
}

struct Report {
    line: String,
}

fn run_one(r: &Run, seconds: f64, dump: Option<&str>) -> Result<Report> {
    let mut s = NativeSession::start(r.cfg.clone())?;
    let info = s.info().clone();
    let mut out = dump.map(std::fs::File::create).transpose()?;
    let t0 = Instant::now();
    let (mut cap_iv, mut au_iv, mut c2au, mut a2au, mut prep, mut encms) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut last_cap, mut last_ready) = (0u64, 0u64);
    let (mut n, mut rep, mut keys, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let warm = Duration::from_millis(500);
    while t0.elapsed().as_secs_f64() < seconds + warm.as_secs_f64() {
        let au = s.next_au()?;
        if let Some(f) = out.as_mut() {
            use std::io::Write;
            f.write_all(&au.data)?;
        }
        if t0.elapsed() < warm {
            last_cap = 0;
            last_ready = 0;
            continue;
        }
        n += 1;
        bytes += u64::try_from(au.data.len())?;
        keys += u64::from(au.keyframe);
        if last_ready != 0 {
            au_iv.push(f64_of(au.ready_t_us - last_ready) / 1000.0);
        }
        last_ready = au.ready_t_us;
        if au.repeat {
            rep += 1;
            continue;
        }
        if last_cap != 0 {
            cap_iv.push((f64_of(au.capture_t_us) - f64_of(last_cap)) / 1000.0);
        }
        last_cap = au.capture_t_us;
        c2au.push((f64_of(au.ready_t_us) - f64_of(au.capture_t_us)) / 1000.0);
        a2au.push(f64_of(au.ready_t_us - au.arrive_t_us) / 1000.0);
        prep.push(f64_of(au.encode_start_t_us - au.arrive_t_us) / 1000.0);
        encms.push(f64_of(au.ready_t_us - au.encode_start_t_us) / 1000.0);
    }
    let el = seconds;
    let maxv = |v: &Vec<f64>| v.iter().copied().fold(f64::NAN, f64::max);
    let cap_max = maxv(&cap_iv);
    let au_max = maxv(&au_iv);
    let line = format!(
        "{:<22} {:>9} {:<7} {:<8} | cap_iv p50 {:5.2} p99 {:5.2} max {:5.1} | au_iv p50 {:5.2} p99 {:5.2} max {:5.1} | cap->AU p50 {:5.2} p99 {:5.2} | arrive->AU p50 {:5.2} p99 {:5.2} (conv+copy {:4.2} enc {:4.2}) | {:5.1} fps {:2} rep/s {:5.1} Mb/s kf {}",
        r.name,
        format!("{}x{}", info.encode_size.0, info.encode_size.1),
        format!("{:?}", info.capture),
        format!("{:?}", info.copy),
        pct(&mut cap_iv, 0.5),
        pct(&mut cap_iv, 0.99),
        cap_max,
        pct(&mut au_iv, 0.5),
        pct(&mut au_iv, 0.99),
        au_max,
        pct(&mut c2au, 0.5),
        pct(&mut c2au, 0.99),
        pct(&mut a2au, 0.5),
        pct(&mut a2au, 0.99),
        pct(&mut prep, 0.5),
        pct(&mut encms, 0.5),
        f64_of(n) / el,
        (f64_of(rep) / el).round(),
        f64_of(bytes) * 8.0 / 1e6 / el,
        keys
    );
    Ok(Report { line })
}

// ---------------------------------------------------------------- animated test window

mod anim {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassW, MSG, PM_REMOVE,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        // SAFETY: DefWindowProcW is called with the arguments Windows passed to this window procedure.
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    /// Moving full-size colour bars + a box on the rect, until `stop`. All window calls happen on this thread.
    pub(super) fn run(x: i32, y: i32, wd: i32, ht: i32, stop: &AtomicBool) {
        // SAFETY: GetModuleHandleW(None) only returns the handle of this executable.
        let hinst = unsafe { GetModuleHandleW(None) }.expect("module handle of the running exe");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("TunnelRezBench"),
            ..Default::default()
        };
        // SAFETY: `wc` is a valid local class description; its strings are static and `wndproc` lives forever.
        unsafe { RegisterClassW(std::ptr::from_ref(&wc)) };
        // SAFETY: the class was registered above; the strings are static and the other arguments are values.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("TunnelRezBench"),
                w!("TunnelRez bench"),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                wd,
                ht,
                None,
                None,
                Some(hinst.into()),
                None,
            )
        }
        .expect("create the bench window");
        let colors =
            [0x00ff_ffffu32, 0x0000_ffff, 0x00ff_ff00, 0x0000_ff00, 0x00ff_00ff, 0x0000_00ff, 0x00ff_0000, 0x0020_2020];
        // SAFETY: CreateSolidBrush takes a plain colour value; the brushes are deleted below.
        let brushes: Vec<HBRUSH> = colors.iter().map(|&c| unsafe { CreateSolidBrush(COLORREF(c)) }).collect();
        // SAFETY: CreateSolidBrush takes a plain colour value; the brush is deleted below.
        let white = unsafe { CreateSolidBrush(COLORREF(0x00ff_ffff)) };
        let mut f = 0i32;
        while !stop.load(Ordering::Relaxed) {
            let mut msg = MSG::default();
            // SAFETY: `msg` is a local out-param; this thread owns the window's message queue.
            while unsafe { PeekMessageW(std::ptr::from_mut(&mut msg), None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: `msg` was just filled by PeekMessageW.
                let _ = unsafe { DispatchMessageW(std::ptr::from_ref(&msg)) };
            }
            // SAFETY: `hwnd` is the live window created above; the DC is released at the end of the frame.
            let dc = unsafe { GetDC(Some(hwnd)) };
            let bw = wd / 8;
            let off = (f * 8) % wd;
            for (i, brush) in (0..9).zip(brushes.iter().cycle()) {
                let x0 = i * bw - off;
                let r = RECT { left: x0.max(0), top: 0, right: (x0 + bw).min(wd), bottom: ht };
                if r.right > r.left {
                    // SAFETY: `dc` and `brush` are live GDI handles; `r` is a local rect.
                    unsafe { FillRect(dc, std::ptr::from_ref(&r), *brush) };
                }
            }
            let bx = (f * 13) % (wd - 200).max(1);
            let by = (f * 7) % (ht - 200).max(1);
            let rect = RECT { left: bx, top: by, right: bx + 200, bottom: by + 200 };
            // SAFETY: `dc` and `white` are live GDI handles; `rect` is a local rect.
            unsafe { FillRect(dc, std::ptr::from_ref(&rect), white) };
            // SAFETY: `dc` came from GetDC on `hwnd` above and is released exactly once.
            unsafe { ReleaseDC(Some(hwnd), dc) };
            f += 1;
            // SAFETY: DwmFlush takes no arguments; it only waits for the next composition.
            let _ = unsafe { DwmFlush() };
        }
        // SAFETY: `hwnd` was created on this thread and is destroyed once.
        let _ = unsafe { DestroyWindow(hwnd) };
        for b in brushes {
            // SAFETY: each brush was created above, is not selected into any DC and is deleted once.
            let _ = unsafe { DeleteObject(b.into()) };
        }
        // SAFETY: `white` was created above, is not selected into any DC and is deleted once.
        let _ = unsafe { DeleteObject(white.into()) };
    }
}

#[expect(clippy::print_stdout, reason = "example binary prints its report")]
fn print_outputs(outs: &[OutputInfo]) {
    for o in outs {
        println!(
            "[{}] {} {}x{} at ({},{}) adapter {} {}{}",
            o.index,
            o.device_name,
            o.width,
            o.height,
            o.x,
            o.y,
            o.adapter,
            if o.primary { " primary" } else { "" },
            if o.virtual_display { " virtual" } else { "" }
        );
    }
}

/// The single run configured by `--capture`, `--gpu` and `--copy` on top of `base`.
fn custom_config(get: &dyn Fn(&str) -> Option<String>, base: NativeConfig) -> Result<NativeConfig> {
    let mut c = base;
    c.capture = match get("--capture").as_deref() {
        None | Some("auto") => CaptureApi::Auto,
        Some("dda") => CaptureApi::Dda,
        Some("wgc") => CaptureApi::Wgc,
        Some(x) => bail!("unknown --capture {x}"),
    };
    c.capture_gpu = match get("--gpu").as_deref() {
        None | Some("auto") => CaptureGpu::Auto,
        Some("encode" | "nvidia") => CaptureGpu::Encode,
        Some("other" | "intel") => CaptureGpu::Other,
        Some(x) => bail!("unknown --gpu {x}"),
    };
    c.copy = match get("--copy").as_deref() {
        None | Some("auto") => CopyPath::Auto,
        Some("direct") => CopyPath::Direct,
        Some("cross") => CopyPath::Cross,
        Some("staging") => CopyPath::Staging,
        Some(x) => bail!("unknown --copy {x}"),
    };
    Ok(c)
}

#[expect(clippy::print_stdout, reason = "example binary prints its report")]
#[expect(clippy::too_many_lines, reason = "a bench script: argument parsing, setup and the measured loop in order")]
fn main() -> Result<()> {
    // Before any DXGI use: lets DDA work on hybrid laptops (outputs owned by the iGPU in this process).
    let _ = prefer_power_saving_gpu();
    // SAFETY: SetProcessDpiAwarenessContext takes a constant and has no pointer arguments.
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let has = |k: &str| args.iter().any(|a| a == k);

    let outs = list_outputs()?;
    if has("--list") {
        print_outputs(&outs);
        return Ok(());
    }
    let output = match get("--output").as_deref() {
        None | Some("virtual") => OutputSel::Virtual,
        Some("primary") => OutputSel::Primary,
        Some(s) if s.parse::<usize>().is_ok() => OutputSel::Index(s.parse()?),
        Some(s) => OutputSel::Name(s.to_string()),
    };
    let o = outs
        .iter()
        .find(|o| match &output {
            OutputSel::Virtual => o.virtual_display,
            OutputSel::Primary => o.primary,
            OutputSel::Index(i) => o.index == *i,
            OutputSel::Name(n) => o.device_name.to_ascii_uppercase().ends_with(&n.to_ascii_uppercase()),
        })
        .ok_or_else(|| anyhow::anyhow!("output {output:?} not found; --list shows them"))?
        .clone();
    let seconds: f64 = get("--seconds").map(|s| s.parse()).transpose()?.unwrap_or(10.0);
    let fps: u32 = get("--fps").map(|s| s.parse()).transpose()?.unwrap_or(60);
    let codec = match get("--codec").as_deref() {
        None | Some("hevc" | "h265") => Codec::Hevc,
        Some("h264") => Codec::H264,
        Some(c) => bail!("unknown codec {c}"),
    };
    let base = NativeConfig { output, codec, fps, ..Default::default() };
    let mbps = get("--bitrate").map(|s| s.parse::<f64>()).transpose()?;
    let with_res = |mut c: NativeConfig, res: Option<(u32, u32)>| {
        if let Some((w, h)) = res {
            c.width = w;
            c.height = h;
        }
        let px = if c.width == 0 {
            u64::try_from(o.width).unwrap_or(0) * u64::try_from(o.height).unwrap_or(0)
        } else {
            u64::from(c.width) * u64::from(c.height)
        };
        c.bitrate_bps = bps_from_mbps(mbps.unwrap_or(if px > 2_100_000 * 2 { 80.0 } else { 30.0 }));
        c
    };

    let mut runs = Vec::new();
    if has("--matrix") {
        for res in [(1920, 1080), (3840, 2160)] {
            for (name, cap, gpu, copy) in [
                ("dda-intel+cross", CaptureApi::Dda, CaptureGpu::Auto, CopyPath::Cross),
                ("dda-intel+staging", CaptureApi::Dda, CaptureGpu::Auto, CopyPath::Staging),
                ("wgc-intel+cross", CaptureApi::Wgc, CaptureGpu::Other, CopyPath::Cross),
                ("wgc-intel+staging", CaptureApi::Wgc, CaptureGpu::Other, CopyPath::Staging),
                ("wgc-nvidia(os copy)", CaptureApi::Wgc, CaptureGpu::Encode, CopyPath::Direct),
            ] {
                let c = NativeConfig { capture: cap, capture_gpu: gpu, copy, ..base.clone() };
                runs.push(Run { name: name.into(), cfg: with_res(c, Some(res)) });
            }
        }
    } else {
        let c = custom_config(&get, base)?;
        let res = get("--res").map(|s| parse_res(&s)).transpose()?;
        runs.push(Run { name: "custom".into(), cfg: with_res(c, res) });
    }

    let stop = Arc::new(AtomicBool::new(false));
    let anim = if has("--animate") || has("--matrix") {
        let (ax, ay, aw, ah, st) = (o.x, o.y, o.width, o.height, Arc::clone(&stop));
        let handle = std::thread::spawn(move || anim::run(ax, ay, aw, ah, &st));
        std::thread::sleep(Duration::from_millis(500));
        Some(handle)
    } else {
        None
    };
    println!(
        "output {} {}x{}, {} fps, {} s per run{}",
        o.device_name,
        o.width,
        o.height,
        fps,
        seconds,
        if anim.is_some() { ", animated" } else { "" }
    );
    println!("cap_iv = interval between captured frames, au_iv = between access units, cap->AU = capture timestamp to AU ready, arrive->AU = frame received by us to AU ready (ms)");
    for r in &runs {
        match run_one(r, seconds, get("--dump").as_deref()) {
            Ok(rep) => println!("{}", rep.line),
            Err(e) => println!("{:<22} FAILED: {e:#}", r.name),
        }
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(t) = anim {
        let _ = t.join();
    }
    Ok(())
}
