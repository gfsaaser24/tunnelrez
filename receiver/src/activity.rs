//! The NativeActivity entry point: window lifecycle, idle screen, foreground/background, Wi-Fi locks.

use crate::pipeline::{lock, Shared};
use crate::wifi::{Hold, WifiLocks};
use crate::{audio, caps, decoders, idle, net, pipeline, proto, util};
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent, WindowManagerFlags};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

/// Process-wide state. NativeActivity may run `android_main` again in the same process (activity recreated),
/// so the listener / output threads and the TCP session outlive a single activity instance.
static SHARED: OnceLock<&'static Shared> = OnceLock::new();

fn shared() -> &'static Shared {
    SHARED.get_or_init(|| {
        let sh: &'static Shared = Box::leak(Box::new(Shared::new()));
        std::thread::Builder::new()
            .name("lrt-output".into())
            .spawn(move || pipeline::output_thread(sh))
            .expect("spawn output thread");
        std::thread::Builder::new()
            .name("lrt-listen".into())
            .spawn(move || net::listener_thread(sh))
            .expect("spawn listener thread");
        std::thread::Builder::new()
            .name("lrt-udp".into())
            .spawn(move || audio::udp_thread(sh))
            .expect("spawn audio UDP thread");
        sh
    })
}

fn draw_idle(sh: &Shared) {
    // Exclusive use of the window: hold the decoder write lock (no codec may connect meanwhile).
    let g = sh.dec_write();
    if g.is_some() {
        return;
    }
    let win = lock(&sh.window);
    let Some(win) = win.as_ref() else { return };
    let addr = format!("{}:{}", util::primary_ip(), proto::PORT);
    let status =
        if sh.connected.load(Ordering::SeqCst) { "PC CONNECTED - WAITING FOR VIDEO" } else { "WAITING FOR PC" };
    let lines = [
        idle::Line { text: "TUNNELREZ", scale: 16, rgb: [0.33, 0.75, 1.0] },
        idle::Line { text: status, scale: 7, rgb: [0.85, 0.88, 0.92] },
        idle::Line { text: &addr, scale: 7, rgb: [0.55, 0.60, 0.68] },
    ];
    match idle::draw(win, &lines) {
        Ok(()) => log::info!("idle screen drawn ({addr})"),
        Err(e) => log::warn!("idle screen: {e}"),
    }
}

/// The app lost the screen (another app is in front, or the window is gone): free the hardware decoder
/// for the other app. The socket stays connected; the session drops frames until we are back.
fn enter_background(sh: &Shared, why: &str) {
    if sh.foreground.swap(false, Ordering::SeqCst) {
        log::info!("background ({why}): releasing decoder");
    }
    sh.destroy_decoder();
}

/// The app is visible again with a window: drop any stale decoder and ask the sender for a keyframe now,
/// so video is back within one round trip instead of waiting for the next periodic keyframe.
fn enter_foreground(sh: &Shared, why: &str) {
    sh.foreground.store(true, Ordering::SeqCst);
    if lock(&sh.window).is_none() {
        return; // InitWindow will finish the job
    }
    log::info!("foreground ({why}): recovering stream");
    sh.recover.store(true, Ordering::SeqCst);
    sh.destroy_decoder(); // takes and releases the decoder write lock; never held while sending below
    sh.force_idr(why);
}

/// A sender is connected and sent a FRAME in the last 500 ms.
fn frames_arriving(sh: &Shared) -> bool {
    if !sh.connected.load(Ordering::SeqCst) {
        return false;
    }
    let last = lock(&sh.stats).last_arrival_us;
    last != 0 && util::now_us().saturating_sub(last) < 500_000
}

/// Build the CAPS message (decoder list) once per process, on its own thread: `MediaCodecList` takes a while.
fn build_caps(app: &AndroidApp, sh: &'static Shared) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // SAFETY: android-activity returns the process's JavaVM pointer, valid for the life of the process.
        let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let sdk = util::get_prop_i32("ro.build.version.sdk", 0);
        let spawned =
            std::thread::Builder::new().name("lrt-caps".into()).spawn(move || match decoders::list(&vm, sdk) {
                Ok(list) => {
                    for d in &list {
                        log::info!(
                            "decoder {} {} max {}x{} @{} fps {} b/s hw={:?} perf={:?}",
                            d.name,
                            d.mime,
                            d.max_width,
                            d.max_height,
                            d.max_fps,
                            d.max_bps,
                            d.hardware,
                            d.perf
                        );
                    }
                    let _ = sh.caps.set(caps::caps_json(&list));
                }
                Err(e) => {
                    log::warn!("decoder list unavailable: {e}");
                    let _ = sh.caps.set(caps::caps_json(&[]));
                }
            });
        if let Err(e) = spawned {
            log::warn!("spawn decoder list thread: {e}");
        }
    });
}

/// Wi-Fi locks, unless `debug.lrt.wifilock=0`.
fn wifi_locks(app: &AndroidApp) -> Option<WifiLocks> {
    if util::get_prop_i32("debug.lrt.wifilock", 1) == 0 {
        log::info!("Wi-Fi locks off (debug.lrt.wifilock=0)");
        return None;
    }
    let sdk = util::get_prop_i32("ro.build.version.sdk", 0);
    match WifiLocks::new(app, sdk) {
        Ok(w) => {
            log::info!("Wi-Fi locks ready (API {sdk}): {w:?}");
            Some(w)
        }
        Err(e) => {
            log::warn!("Wi-Fi locks unavailable: {e}");
            None
        }
    }
}

fn on_event(app: &AndroidApp, sh: &Shared, ev: &PollEvent<'_>) -> bool {
    match ev {
        PollEvent::Main(MainEvent::InitWindow { .. }) => {
            let w = app.native_window();
            log::info!("window ready: {:?}", w.as_ref().map(|w| (w.width(), w.height())));
            *lock(&sh.window) = w;
            sh.idle_dirty.store(true, Ordering::SeqCst);
            // A new window means the activity is visible; any old decoder belonged to the old surface.
            enter_foreground(sh, "window ready");
        }
        PollEvent::Main(MainEvent::TerminateWindow { .. }) => {
            // Must stop using the window before returning from this callback.
            log::info!("window lost");
            enter_background(sh, "window lost");
            *lock(&sh.window) = None;
        }
        PollEvent::Main(MainEvent::WindowResized { .. } | MainEvent::RedrawNeeded { .. }) => {
            sh.idle_dirty.store(true, Ordering::SeqCst);
        }
        PollEvent::Main(MainEvent::Pause) => log::info!("paused"),
        PollEvent::Main(MainEvent::Stop) => {
            log::info!("stopped");
            enter_background(sh, "stopped");
        }
        PollEvent::Main(MainEvent::Start) => {
            log::info!("started");
            if !sh.foreground.load(Ordering::SeqCst) {
                enter_foreground(sh, "started");
            }
        }
        PollEvent::Main(MainEvent::Resume { .. }) => {
            log::info!("resumed");
            if !sh.foreground.load(Ordering::SeqCst) {
                enter_foreground(sh, "resumed");
            }
        }
        PollEvent::Main(MainEvent::GainedFocus) => {
            log::info!("focus gained");
            if !sh.foreground.load(Ordering::SeqCst) {
                enter_foreground(sh, "focus gained");
            }
        }
        PollEvent::Main(MainEvent::LostFocus) => log::info!("focus lost"),
        PollEvent::Main(MainEvent::Destroy) => {
            log::info!("destroy");
            sh.destroy_decoder();
            *lock(&sh.window) = None;
            return true;
        }
        PollEvent::Main(MainEvent::InputAvailable) => {
            if let Ok(mut it) = app.input_events_iter() {
                // Leave everything unhandled so BACK / HOME keep their default behavior.
                while it.next(|_| InputStatus::Unhandled) {}
            }
        }
        _ => {}
    }
    false
}

#[no_mangle]
#[expect(
    clippy::no_mangle_with_rust_abi,
    clippy::needless_pass_by_value,
    reason = "android-activity calls `android_main(AndroidApp)` by name with the Rust ABI"
)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_tag("LRT").with_max_level(log::LevelFilter::Info));
    std::panic::set_hook(Box::new(|p| log::error!("panic: {p}")));

    let ips = util::local_ipv4();
    log::info!("TunnelRez receiver starting. TV IPs: {ips:?}; listening on port {}", proto::PORT);
    log::info!("Waiting for PC on {}:{}", util::primary_ip(), proto::PORT);

    app.set_window_flags(
        WindowManagerFlags::KEEP_SCREEN_ON | WindowManagerFlags::FULLSCREEN,
        WindowManagerFlags::empty(),
    );
    let sh = shared();
    sh.idle_dirty.store(true, Ordering::SeqCst);
    build_caps(&app, sh);
    let mut wifi = wifi_locks(&app);

    let mut quit = false;
    while !quit {
        app.poll_events(Some(Duration::from_millis(200)), |ev| quit |= on_event(&app, sh, &ev));
        // Hold the Wi-Fi locks while a sender is connected and we are on screen; release in the background.
        if let Some(w) = wifi.as_mut() {
            let streaming = !quit && sh.connected.load(Ordering::SeqCst) && sh.visible();
            w.set(if streaming { Hold::Held } else { Hold::Released });
        }
        // Skip the idle screen while video frames are arriving: a decoder is about to take the window, and
        // drawing would only delay it. It is drawn later if the frames stop.
        if !quit
            && sh.idle_dirty.load(Ordering::SeqCst)
            && sh.foreground.load(Ordering::SeqCst)
            && !sh.has_decoder()
            && !frames_arriving(sh)
        {
            sh.idle_dirty.store(false, Ordering::SeqCst);
            draw_idle(sh);
        }
    }
}
