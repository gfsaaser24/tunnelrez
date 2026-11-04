//! Diagnostics: which adapter owns which output, and which capture paths work from this process.
//!   cargo run --release --example probe [-- --pref 0|1|2]
use std::time::{Duration, Instant};
use tunnelrez_native::{capture::dda::DdaCapture, capture::wgc::WgcCapture, clock, d3d, gpupref};

/// Index of quantile `q` (0..=1) in a sorted slice of `len` (> 0) items, rounded down.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float quantile math on small sample counts; truncation toward zero is the intended rounding"
)]
fn quantile_index(len: usize, q: f64) -> usize {
    ((len - 1) as f64 * q) as usize
}

#[expect(clippy::print_stdout, reason = "example binary prints its report")]
#[expect(clippy::too_many_lines, reason = "a probe script: one pass over the outputs, top to bottom")]
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let exe = std::env::current_exe()?;
    if let Some(i) = args.iter().position(|a| a == "--pref") {
        let p: u32 = args[i + 1].parse()?;
        gpupref::set(&exe, p)?;
        println!("set GPU preference {p} for this exe (before first DXGI use)");
    }
    println!("GPU preference for this exe: {:?}", gpupref::get(&exe));
    for a in d3d::adapters()? {
        println!("adapter {a:?}");
    }
    let outs = d3d::outputs()?;
    for o in &outs {
        let dda = d3d::create_device(&o.adapter).and_then(|dev| {
            let mut c = DdaCapture::new(&dev.device, &o.output)?;
            let t = Instant::now();
            let mut n = 0;
            while t.elapsed() < Duration::from_millis(300) {
                if let Some(_f) = c.next(50)? {
                    n += 1;
                }
            }
            Ok(format!("ok {}x{} ({n} updates in 300 ms)", c.width, c.height))
        });
        println!(
            "output {} {}x{} at ({},{}) on adapter {} {}  DDA: {}",
            o.device_name,
            o.width,
            o.height,
            o.x,
            o.y,
            o.adapter.index,
            d3d::vendor_name(o.adapter.vendor_id),
            match dda {
                Ok(s) => s,
                Err(e) => format!("FAIL {e:#}"),
            }
        );
    }
    let want = args.iter().position(|a| a == "--wgc").map(|i| args[i + 1].clone());
    for o in &outs {
        if let Some(w) = &want {
            if !o.device_name.ends_with(w.as_str()) {
                continue;
            }
        }
        for a in d3d::adapters()?.iter().filter(|a| !a.software) {
            let r = (|| -> anyhow::Result<String> {
                let dev = d3d::create_device(a)?;
                let mut c = WgcCapture::new(&dev.device, o.monitor)?;
                let t = Instant::now();
                let mut ivs = Vec::new();
                let mut lags = Vec::new();
                let mut last = 0u64;
                let mut desc = String::new();
                while t.elapsed() < Duration::from_secs(2) {
                    if let Some(f) = c.next(100)? {
                        let now = clock::now_us();
                        lags.push(now.cast_signed() - f.t_us.cast_signed());
                        if last != 0 {
                            ivs.push(f.t_us.cast_signed() - last.cast_signed());
                        }
                        last = f.t_us;
                        if desc.is_empty() {
                            let d = d3d::tex_desc(&f.tex);
                            // SAFETY: GetDevice on a live texture.
                            let dv = unsafe { f.tex.GetDevice()? };
                            let luid = d3d::device_luid(&dv)?;
                            desc = format!(
                                "{}x{} fmt {:?} misc {:#x} on luid {:#x}",
                                d.Width,
                                d.Height,
                                d.Format.0,
                                d.MiscFlags,
                                d3d::luid_u64(luid)
                            );
                        }
                    }
                }
                ivs.sort_unstable();
                lags.sort_unstable();
                let p = |v: &Vec<i64>, q: f64| {
                    if v.is_empty() {
                        0
                    } else {
                        v[quantile_index(v.len(), q)]
                    }
                };
                Ok(format!(
                    "{} frames in 2 s, interval p50 {} us p99 {} us, now-t_us p50 {} us p99 {} us, tex {}",
                    lags.len(),
                    p(&ivs, 0.5),
                    p(&ivs, 0.99),
                    p(&lags, 0.5),
                    p(&lags, 0.99),
                    desc
                ))
            })();
            println!(
                "WGC {} with device on {} {}: {}",
                o.device_name,
                a.index,
                d3d::vendor_name(a.vendor_id),
                match r {
                    Ok(s) => s,
                    Err(e) => format!("FAIL {e:#}"),
                }
            );
        }
    }
    Ok(())
}
