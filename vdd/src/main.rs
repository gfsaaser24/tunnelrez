//! `tunnelrez-vdd`: plug one Parsec virtual monitor for the TV and hold it until Ctrl+C.
//!
//!   tunnelrez-vdd run [1920x1080@60] [--hold SECS]   plug, set the mode, keep alive; Ctrl+C (or SECS) unplugs
//!   tunnelrez-vdd list                               show the plugged Parsec virtual displays
//!   tunnelrez-vdd mode NAME 1920x1080@60             attach a plugged display and set its mode (the reuse path)
//!   tunnelrez-vdd unplug-all                         unplug every virtual display, whoever plugged it

use std::sync::mpsc;
use std::time::Duration;

use tunnelrez_vdd::{ensure_mode, unplug_all, vdd_displays, Mode, VirtualDisplay};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("list") => {
            let displays = vdd_displays();
            if displays.is_empty() {
                println!("no Parsec virtual displays plugged");
            }
            for d in displays {
                println!("{} {}", d.name, if d.attached { "attached" } else { "detached" });
            }
        }
        Some("mode") => {
            let (Some(name), Some(mode)) = (args.get(1), args.get(2)) else { fail("use: mode NAME WxH@HZ") };
            let mode: Mode = mode.parse().unwrap_or_else(|e: tunnelrez_vdd::VddError| fail(&e.to_string()));
            ensure_mode(name, mode).unwrap_or_else(|e| fail(&e.to_string()));
            println!("{name} shows {mode}");
        }
        Some("unplug-all") => {
            unplug_all().unwrap_or_else(|e| fail(&e.to_string()));
            println!("every virtual display unplugged");
        }
        Some("run") | None => {
            let mode: Mode = match args.get(1).map_or_else(|| "1920x1080@60".parse(), |s| s.parse()) {
                Ok(m) => m,
                Err(e) => fail(&e.to_string()),
            };
            let hold = match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("--hold"), Some(s)) => {
                    Some(Duration::from_secs(s.parse().unwrap_or_else(|_| fail("bad --hold"))))
                }
                (None, _) => None,
                _ => fail("use: run [WxH@HZ] [--hold SECS]"),
            };
            let (tx, rx) = mpsc::channel();
            ctrlc::set_handler(move || {
                let _ = tx.send(());
            })
            .unwrap_or_else(|e| fail(&format!("Ctrl+C handler: {e}")));

            let vd = VirtualDisplay::plug(mode).unwrap_or_else(|e| fail(&e.to_string()));
            println!(
                "plugged virtual display index {} as {} at {}x{}@{} (driver minor version {:?}); Ctrl+C to unplug",
                vd.index(),
                vd.device_name.as_deref().unwrap_or("<not attached yet>"),
                mode.width,
                mode.height,
                mode.hz,
                vd.driver_version
            );
            let _ = hold.map_or_else(|| rx.recv().map_err(|_| ()), |d| rx.recv_timeout(d).map_err(|_| ()));
            drop(vd);
            println!("unplugged");
        }
        Some(other) => fail(&format!(
            "unknown command {other:?}; use: run [WxH@HZ] [--hold SECS] | list | mode NAME WxH@HZ | unplug-all"
        )),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("tunnelrez-vdd: {msg}");
    std::process::exit(1);
}
