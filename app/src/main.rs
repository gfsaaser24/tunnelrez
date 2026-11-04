//! `TunnelRez` tray app: Tauri setup, the tray icon and menu, the flyout panel, and the commands the panel calls.

// A GUI-subsystem binary in every build: no console window, ever.
#![windows_subsystem = "windows"]

mod core;
mod gate;
mod logger;
mod platform;
mod runbook;
mod settings;
mod state;
mod stream;
mod testrun;
mod tv;
mod tvcaps;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, RunEvent, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent, Wry,
};

use crate::core::{Core, LogStatus, Snapshot, Ui};
use crate::platform::{Autostart, Rect, WebviewMemory};
use crate::settings::Mode;
use crate::state::State;
use crate::stream::LiveStats;
use crate::testrun::TestPage;
use crate::tv::Reach;

const PANEL: &str = "panel";
/// The test page window (see `testrun`).
const TEST_PAGE: &str = "testpage";
const TRAY: &str = "main";
/// Panel width in CSS px; the height follows the content (`fit_panel`).
const PANEL_WIDTH: f64 = 360.0;
/// Gap between the panel and the taskbar / screen edge, in CSS px (Win11 flyouts use 12).
const PANEL_MARGIN: f64 = 12.0;
/// A tray click this soon after the panel hid (because that very click took the focus away) must not reopen it.
const REOPEN_GUARD: Duration = Duration::from_millis(300);
/// Save the kept-open panel's position once a drag has been still this long (not once per pixel).
const POS_SETTLE: Duration = Duration::from_millis(400);

// ------------------------------------------------------------------------------------------------ tray icons

macro_rules! tray_png {
    ($state:literal, $theme:literal) => {
        [
            include_bytes!(concat!("../icons/tray-", $state, "-", $theme, "-16.png")).as_slice(),
            include_bytes!(concat!("../icons/tray-", $state, "-", $theme, "-20.png")).as_slice(),
            include_bytes!(concat!("../icons/tray-", $state, "-", $theme, "-24.png")).as_slice(),
            include_bytes!(concat!("../icons/tray-", $state, "-", $theme, "-32.png")).as_slice(),
        ]
    };
}

/// [state][theme][size]: state idle/streaming/alert, theme dark/light taskbar, size 16/20/24/32.
static TRAY_PNGS: [[[&[u8]; 4]; 2]; 3] = [
    [tray_png!("idle", "dark"), tray_png!("idle", "light")],
    [tray_png!("streaming", "dark"), tray_png!("streaming", "light")],
    [tray_png!("alert", "dark"), tray_png!("alert", "light")],
];

fn tray_icon(state: &State) -> Option<Image<'static>> {
    let s = match state {
        State::Streaming { .. } => 1,
        State::Error { .. } => 2,
        _ => 0,
    };
    let theme = usize::from(platform::taskbar_is_light());
    let size = match platform::tray_icon_px() {
        16 => 0,
        20 => 1,
        24 => 2,
        _ => 3,
    };
    Image::from_bytes(TRAY_PNGS[s][theme][size]).ok()
}

// ------------------------------------------------------------------------------------------------ UI bridge

struct TrayItems {
    extend: MenuItem<Wry>,
    mirror: MenuItem<Wry>,
    stop: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
}

/// The Tauri side of [`Ui`]: events to the panel, icon/tooltip/menu to the tray.
struct TauriUi {
    app: AppHandle,
    items: Arc<OnceLock<TrayItems>>,
    panel: Arc<Panel>,
    /// The icon currently shown, to avoid resetting it every second. Only used on the event thread.
    icon_kind: Arc<Mutex<Option<(u8, bool)>>>,
    /// The test page window has finished loading its page.
    test_loaded: Arc<AtomicBool>,
    /// The event loop's thread (where `setup` runs).
    ui_thread: std::thread::ThreadId,
}

impl Ui for TauriUi {
    fn snapshot(&self, snap: &Snapshot) {
        let _ = self.app.emit_to(PANEL, "snapshot", snap);
    }

    fn stats(&self, stats: &LiveStats) {
        let _ = self.app.emit_to(PANEL, "stats", stats);
    }

    fn tray(&self, state: &State, tooltip: &str) {
        let Some(tray) = self.app.tray_by_id(TRAY) else { return };
        let _ = tray.set_tooltip(Some(tooltip));
        // The icon is decided and set in one step on the event thread, from the state current then (not the
        // one this call was given): publications from several threads can arrive out of order, and a cached
        // icon is never re-set, so the last one to run must show the latest state. Not waited for: a worker
        // must not block on the event thread while holding `ops` (see `quit_app`).
        let (app, cache, given) = (self.app.clone(), Arc::clone(&self.icon_kind), state.clone());
        let _ = self.app.run_on_main_thread(move || {
            let state = app.try_state::<Arc<Core>>().map_or(given, |c| c.state());
            apply_icon(&app, &cache, &state);
        });
        if let Some(items) = self.items.get() {
            let active = state.is_active();
            let can_start = matches!(state, State::Idle | State::Error { .. });
            let _ = items.extend.set_enabled(can_start);
            let _ = items.mirror.set_enabled(can_start);
            let _ = items.stop.set_enabled(active);
            let _ = items.autostart.set_checked(platform::autostart_enabled());
        }
    }

    fn show_panel(&self) {
        self.panel.show(&self.app);
    }

    fn log_status(&self, status: &LogStatus) {
        let _ = self.app.emit_to(PANEL, "log", status);
    }

    fn test_page_show(&self, r: Rect) -> anyhow::Result<()> {
        let size = PhysicalSize::new(u32::try_from(r.right - r.left)?, u32::try_from(r.bottom - r.top)?);
        let w = if let Some(w) = self.app.get_webview_window(TEST_PAGE) {
            w
        } else {
            self.test_loaded.store(false, Ordering::SeqCst);
            let loaded = Arc::clone(&self.test_loaded);
            // Borderless, no shadow (it would add a frame around the rect), never focused, not in the taskbar,
            // above other windows on the TV screen. Hidden until it is on that screen.
            WebviewWindowBuilder::new(&self.app, TEST_PAGE, WebviewUrl::App(testrun::PAGE.into()))
                .title("TunnelRez test page")
                .decorations(false)
                .shadow(false)
                .resizable(false)
                .maximizable(false)
                .minimizable(false)
                .skip_taskbar(true)
                .always_on_top(true)
                .focused(false)
                .visible(false)
                .on_page_load(move |_, p| {
                    if p.event() == PageLoadEvent::Finished {
                        loaded.store(true, Ordering::SeqCst);
                    }
                })
                .build()?
        };
        // Physical px: the TV screen may have another scale than the panel's. A move across scales can resize
        // the window; the test run checks the result and moves it again.
        w.set_position(PhysicalPosition::new(r.left, r.top))?;
        w.set_size(size)?;
        w.show()?;
        Ok(())
    }

    fn test_page(&self) -> Option<TestPage> {
        let w = self.app.get_webview_window(TEST_PAGE)?;
        let (p, s) = (w.outer_position().ok()?, w.outer_size().ok()?);
        let rect = Rect {
            left: p.x,
            top: p.y,
            right: p.x.saturating_add(i32::try_from(s.width).ok()?),
            bottom: p.y.saturating_add(i32::try_from(s.height).ok()?),
        };
        let loaded = self.test_loaded.load(Ordering::SeqCst) && w.is_visible().unwrap_or(false);
        Some(TestPage { rect, loaded })
    }

    fn test_page_close(&self) {
        if let Some(w) = self.app.get_webview_window(TEST_PAGE) {
            // `destroy` only queues the close on the event loop. Off that thread, wait until the window is really
            // gone, so the monitor under it is not unplugged first (Windows would move it onto another screen).
            let _ = w.destroy();
            if !self.on_ui_thread() {
                let until = Instant::now() + TEST_PAGE_CLOSE_WAIT;
                while self.app.get_webview_window(TEST_PAGE).is_some() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if self.app.get_webview_window(TEST_PAGE).is_some() {
                    crate::core::log("the test page did not close in time; the monitor is released anyway");
                }
            }
        }
        self.test_loaded.store(false, Ordering::SeqCst);
    }

    fn on_ui_thread(&self) -> bool {
        std::thread::current().id() == self.ui_thread
    }
}

/// How long closing the test page may take before teardown goes on anyway. Bounded on purpose: Stop and Quit must
/// finish even when the event loop is stuck, and an event loop that has not run a queued destroy in this long cannot
/// move the window either.
const TEST_PAGE_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Show the tray icon for `state` unless it is already shown. Event thread only, so the cache and the icon on
/// screen change together.
fn apply_icon(app: &AppHandle, cache: &Mutex<Option<(u8, bool)>>, state: &State) {
    let Some(tray) = app.tray_by_id(TRAY) else { return };
    let kind = (
        match state {
            State::Streaming { .. } => 1,
            State::Error { .. } => 2,
            _ => 0,
        },
        platform::taskbar_is_light(),
    );
    let mut current = cache.lock().expect("icon lock poisoned");
    if *current != Some(kind) {
        if let Some(icon) = tray_icon(state) {
            let _ = tray.set_icon(Some(icon));
        }
        *current = Some(kind);
    }
}

// ------------------------------------------------------------------------------------------------ panel

/// The panel window. Two behaviours, picked by the "Keep open" pin (`Settings::keep_panel_open`):
///
/// - **Off:** a flyout like Wi-Fi's: anchored above the tray, always on top, hides on blur.
/// - **On:** a normal window: stays open on blur, the user drags it by its header, its position is saved and
///   restored (clamped to a visible work area). Esc and the tray icon still hide it.
#[derive(Default)]
struct Panel {
    /// Tray icon rectangle from the last click (physical px).
    anchor: Mutex<Option<Rect>>,
    hidden_at: Mutex<Option<Instant>>,
    /// When the panel last lost the focus (a tray click takes it away before the click arrives).
    blurred_at: Mutex<Option<Instant>>,
    /// Show the panel once the page has loaded (first run, or launched from the Start menu).
    show_when_ready: Mutex<bool>,
}

/// `t` is less than [`REOPEN_GUARD`] ago.
fn just(t: &Mutex<Option<Instant>>) -> bool {
    t.lock().expect("instant lock poisoned").is_some_and(|t| t.elapsed() < REOPEN_GUARD)
}

fn keep_open(app: &AppHandle) -> bool {
    app.try_state::<Arc<Core>>().is_some_and(|c| c.keep_panel_open())
}

/// Move a `w`×`h` box with its top-left at (`x`, `y`) the least distance that puts it inside `work`. A box larger
/// than `work` keeps its top-left corner visible.
fn clamp_into(work: Rect, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    let x = x.min(work.right - w).max(work.left);
    let y = y.min(work.bottom - h).max(work.top);
    (x, y)
}

impl Panel {
    fn window(app: &AppHandle) -> Option<WebviewWindow> {
        app.get_webview_window(PANEL)
    }

    /// Apply the "Keep open" behaviour to the window: a normal window when kept open, a topmost flyout when not.
    fn apply_keep_open(&self, app: &AppHandle) {
        let Some(w) = Self::window(app) else { return };
        let on = keep_open(app);
        let _ = w.set_always_on_top(!on);
        if !on {
            // Back to the flyout: snap to the tray.
            self.place(app, &w);
        }
    }

    fn anchor(&self, app: &AppHandle) -> Option<Rect> {
        if let Some(r) = *self.anchor.lock().expect("anchor lock poisoned") {
            return Some(r);
        }
        let rect = app.tray_by_id(TRAY)?.rect().ok()??;
        let pos = rect.position.to_physical::<i32>(1.0);
        let size = rect.size.to_physical::<i32>(1.0);
        Some(Rect { left: pos.x, top: pos.y, right: pos.x + size.width, bottom: pos.y + size.height })
    }

    /// Put the panel next to the tray, like the Wi-Fi flyout: right-aligned above a bottom taskbar, and the
    /// matching corner for a top, left or right taskbar.
    fn place(&self, app: &AppHandle, w: &WebviewWindow) {
        // Place the visible (client) area: the window rect also holds Windows' invisible resize frame.
        let (Ok(inner), Ok(ipos), Ok(opos)) = (w.inner_size(), w.inner_position(), w.outer_position()) else {
            return;
        };
        let px = |v: u32| i32::try_from(v).unwrap_or(0);
        let (sw, sh) = (px(inner.width), px(inner.height));
        let (frame_left, frame_top) = (ipos.x - opos.x, ipos.y - opos.y);
        if let Some(core) = app.try_state::<Arc<Core>>().filter(|c| c.keep_panel_open()) {
            // Kept open: stay where the user put it (where it is now while shown, the saved spot when it opens),
            // pulled back into the work area of the nearest monitor. Nothing saved yet: the tray spot below.
            let here = w.is_visible().unwrap_or(false).then_some((opos.x, opos.y));
            if let Some((ox, oy)) = here.or_else(|| core.panel_pos()) {
                let (x, y) = (ox + frame_left, oy + frame_top);
                let Some((_, work)) = platform::monitor_at(x + sw / 2, y + sh / 2) else { return };
                let (x, y) = clamp_into(work, x, y, sw, sh);
                if (x - frame_left, y - frame_top) != (opos.x, opos.y) {
                    let _ = w.set_position(PhysicalPosition::new(x - frame_left, y - frame_top));
                }
                return;
            }
        }
        let (cx, cy) = match self.anchor(app) {
            Some(r) => (i32::midpoint(r.left, r.right), i32::midpoint(r.top, r.bottom)),
            None => (i32::MAX / 2, i32::MAX / 2),
        };
        let Some((mon, work)) = platform::monitor_at(cx, cy) else { return };
        let scale = w.scale_factor().unwrap_or(1.0);
        // Rounded once to whole px: the gap stays exact at every DPI.
        #[expect(clippy::cast_possible_truncation, clippy::as_conversions, reason = "a few px, far inside i32")]
        let m = (PANEL_MARGIN * scale).round() as i32;
        let clamp_y = |y: i32| y.clamp(work.top + m, (work.bottom - m - sh).max(work.top + m));
        let (x, y) = if work.top > mon.top {
            (work.right - m - sw, work.top + m)
        } else if work.left > mon.left {
            (work.left + m, clamp_y(cy - sh / 2))
        } else if work.right < mon.right {
            (work.right - m - sw, clamp_y(cy - sh / 2))
        } else {
            // Bottom taskbar, or an auto-hiding one (treated as bottom).
            (work.right - m - sw, work.bottom - m - sh)
        };
        let _ = w.set_position(PhysicalPosition::new(x - frame_left, y - frame_top));
    }

    fn show(&self, app: &AppHandle) {
        let Some(w) = Self::window(app) else { return };
        self.place(app, &w);
        platform::webview_memory(&w, WebviewMemory::Normal);
        let _ = w.show();
        let _ = w.set_focus();
        if let Some(core) = app.try_state::<Arc<Core>>() {
            core.panel_shown();
            let _ = app.emit_to(PANEL, "snapshot", core.snapshot());
            if core.mode() == Mode::Mirror {
                let core = Arc::clone(&core);
                std::thread::spawn(move || core.refresh_monitors());
            }
        }
        let _ = app.emit_to(PANEL, "panel-shown", ());
    }

    fn hide(&self, app: &AppHandle) {
        let Some(w) = Self::window(app) else { return };
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
            platform::webview_memory(&w, WebviewMemory::Low);
            *self.hidden_at.lock().expect("hidden_at lock poisoned") = Some(Instant::now());
        }
        if let Some(core) = app.try_state::<Arc<Core>>() {
            core.panel_hidden();
        }
    }

    fn toggle(&self, app: &AppHandle) {
        let Some(w) = Self::window(app) else { return };
        if w.is_visible().unwrap_or(false) {
            // Kept open and left behind other windows a while ago: the click brings it back instead of hiding it.
            // (Had it been in front, this very click would just have taken its focus.)
            let in_front = w.is_focused().unwrap_or(false) || just(&self.blurred_at);
            if keep_open(app) && !in_front {
                let _ = w.set_focus();
            } else {
                self.hide(app);
            }
        } else if !just(&self.hidden_at) {
            self.show(app);
        }
    }

    /// Focus left the panel: the flyout hides; a kept-open panel stays.
    fn blurred(&self, app: &AppHandle) {
        *self.blurred_at.lock().expect("blurred_at lock poisoned") = Some(Instant::now());
        if !keep_open(app) {
            self.hide(app);
        }
    }
}

// ------------------------------------------------------------------------------------------------ commands

type CoreState<'a> = tauri::State<'a, Arc<Core>>;

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn get_snapshot(core: CoreState<'_>) -> Snapshot {
    core.snapshot()
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn start(core: CoreState<'_>, mode: Option<Mode>) {
    let mode = mode.unwrap_or_else(|| core.mode());
    core.start(mode);
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn stop(core: CoreState<'_>) {
    core.stop();
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn set_setting(core: CoreState<'_>, key: &str, value: Value) -> bool {
    core.set_setting(key, value)
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn finish_setup(core: CoreState<'_>, host: &str) {
    core.finish_setup(host);
}

#[tauri::command]
async fn test_tv(core: CoreState<'_>, host: String) -> Result<Reach, String> {
    if core.state().is_active() {
        // A test connection would replace the running stream on the TV.
        return Ok(Reach::Ready);
    }
    tauri::async_runtime::spawn_blocking(move || tv::check(&host)).await.map_err(|e| e.to_string())
}

/// Open a Microsoft Store page for the HEVC Video Extensions: `free` (from Device Manufacturer) or `paid`.
#[tauri::command]
fn open_store(which: &str) -> Result<(), String> {
    let (store, web) = if which == "paid" {
        (gate::HEVC_STORE_PAID, gate::HEVC_WEB_PAID)
    } else {
        (gate::HEVC_STORE_FREE, gate::HEVC_WEB_FREE)
    };
    platform::open_uri(store).or_else(|_| platform::open_uri(web)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn open_tv_app(host: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || tv::open_app(&host))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
#[expect(clippy::fn_params_excessive_bools, reason = "the panel sends this as a plain JSON bool")]
fn set_autostart(app: AppHandle, core: CoreState<'_>, on: bool) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    platform::set_autostart(&exe, Autostart::from(on)).map_err(|e| format!("{e:#}"))?;
    let enabled = platform::autostart_enabled();
    if let Some(items) = app.try_state::<Arc<OnceLock<TrayItems>>>().and_then(|i| i.get().map(|i| i.autostart.clone()))
    {
        let _ = items.set_checked(enabled);
    }
    let _ = app.emit_to(PANEL, "snapshot", core.snapshot());
    Ok(enabled)
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn refresh_monitors(core: CoreState<'_>) {
    let core = Arc::clone(&core);
    std::thread::spawn(move || core.refresh_monitors());
}

/// The page measured its content: size the window to it and keep it anchored to the taskbar.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn fit_panel(app: AppHandle, panel: tauri::State<'_, Arc<Panel>>, height: f64) {
    let Some(w) = Panel::window(&app) else { return };
    // Never taller than the work area of the monitor the panel is on; the page scrolls the rest.
    let max = w
        .outer_position()
        .ok()
        .zip(w.outer_size().ok())
        .and_then(|(p, sz)| {
            let half = |v: u32| i32::try_from(v / 2).unwrap_or(0);
            platform::monitor_at(p.x + half(sz.width), p.y + half(sz.height))
        })
        .map_or(900.0, |(_, work)| {
            let scale = w.scale_factor().unwrap_or(1.0);
            f64::from(work.bottom - work.top) / scale - 2.0 * PANEL_MARGIN
        });
    let height = height.clamp(200.0, max.max(200.0)).ceil();
    let _ = w.set_size(LogicalSize::new(PANEL_WIDTH, height));
    panel.place(&app, &w);
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn hide_panel(app: AppHandle, panel: tauri::State<'_, Arc<Panel>>) {
    panel.hide(&app);
}

/// The "Keep open" pin in the panel header. Returns the new value.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
#[expect(clippy::fn_params_excessive_bools, reason = "the panel sends this as a plain JSON bool")]
fn set_keep_open(app: AppHandle, core: CoreState<'_>, panel: tauri::State<'_, Arc<Panel>>, on: bool) -> bool {
    core.set_setting("keep_panel_open", Value::Bool(on));
    let on = core.keep_panel_open();
    panel.apply_keep_open(&app);
    on
}

/// The page has loaded and rendered its first snapshot.
/// The panel's Log switch: on starts a new log file, off stops and saves it.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
#[expect(clippy::fn_params_excessive_bools, reason = "the panel sends this as a plain JSON bool")]
fn set_logging(core: CoreState<'_>, on: bool) {
    if on {
        core.start_log();
    } else {
        core.stop_log();
    }
}

/// The panel's Test button: on opens the moving test page full screen on the TV screen and logs once it fills it;
/// off closes the page and stops that log.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
#[expect(clippy::fn_params_excessive_bools, reason = "the panel sends this as a plain JSON bool")]
fn set_test(core: CoreState<'_>, on: bool) {
    if on {
        core.start_test();
    } else {
        core.stop_test();
    }
}

/// "Add to run book": the last log into `tools/runbook/add-run.ps1`, with what was on screen (`content`, may be
/// empty). The result comes as a `log` event.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn add_to_runbook(core: CoreState<'_>, content: Option<String>) {
    core.add_to_runbook(content.unwrap_or_default());
}

/// "Install TV speaker": the install runs on its own thread (the sender shows the Windows prompt); the panel follows
/// it in the snapshots.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn install_tv_speaker(core: CoreState<'_>) {
    core.install_tv_speaker();
}

/// "Repair" or "Install" on the TV monitor: runs on its own thread (the sender shows the Windows prompt); the panel
/// follows it in the snapshots.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn fix_tv_monitor(core: CoreState<'_>) {
    core.fix_tv_monitor();
}

/// Re-read the playback devices and the TV speaker's and TV monitor's status, off the UI thread.
#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn refresh_audio(core: CoreState<'_>) {
    let core = Arc::clone(&core);
    std::thread::spawn(move || core.refresh_audio());
}

/// Open the logs folder in Explorer.
#[tauri::command]
fn open_logs() -> Result<(), String> {
    let dir = settings::Settings::dir().join("logs");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    platform::open_folder(&dir).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn ui_ready(app: AppHandle, panel: tauri::State<'_, Arc<Panel>>) {
    let show = std::mem::take(&mut *panel.show_when_ready.lock().expect("show lock poisoned"));
    if show {
        panel.show(&app);
    }
}

#[tauri::command]
#[expect(clippy::needless_pass_by_value, reason = "Tauri injects `State` and `AppHandle` into commands by value")]
fn quit(app: AppHandle) {
    quit_app(&app);
}

/// Set once quitting has begun (later quit requests are ignored).
static QUIT_STARTED: AtomicBool = AtomicBool::new(false);
/// Set once [`Core::shutdown`] has finished: the exit may go ahead.
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

/// Stop the sender, unplug the monitor, then exit. Never blocks the caller: the teardown runs on its own thread,
/// because it waits for workers (start, sender supervisor) that can themselves be waiting for a tray or menu
/// update on the event thread. Quitting from the event thread would hang the app with the monitor still plugged.
fn quit_app(app: &AppHandle) {
    if QUIT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    let spawned = std::thread::Builder::new().name("quit".into()).spawn(move || {
        if let Some(core) = handle.try_state::<Arc<Core>>() {
            core.shutdown();
        }
        SHUT_DOWN.store(true, Ordering::SeqCst);
        handle.exit(0);
    });
    if spawned.is_err() {
        // No thread: tear down here; it can only block if a worker is mid-update, which is rare.
        if let Some(core) = app.try_state::<Arc<Core>>() {
            core.shutdown();
        }
        SHUT_DOWN.store(true, Ordering::SeqCst);
        app.exit(0);
    }
}

// ------------------------------------------------------------------------------------------------ command line

/// `tunnelrez.exe [--extend | --mirror | --stop | --quit | --autostart]`. Sent to the running copy when there is
/// one (single instance), so shortcuts and scripts can drive the app. No argument opens the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cli {
    Show,
    Start(Mode),
    Stop,
    Quit,
    /// Started by Windows at logon: stay in the tray.
    Autostart,
}

fn parse_cli(args: &[String]) -> Cli {
    args.iter()
        .skip(1)
        .find_map(|a| match a.as_str() {
            "--extend" => Some(Cli::Start(Mode::Extend)),
            "--mirror" => Some(Cli::Start(Mode::Mirror)),
            "--stop" => Some(Cli::Stop),
            "--quit" => Some(Cli::Quit),
            "--autostart" => Some(Cli::Autostart),
            _ => None,
        })
        .unwrap_or(Cli::Show)
}

fn run_cli(app: &AppHandle, panel: &Panel, cli: Cli) {
    let Some(core) = app.try_state::<Arc<Core>>() else { return };
    match cli {
        Cli::Show => panel.show(app),
        Cli::Start(mode) => core.start(mode),
        Cli::Stop => core.stop(),
        Cli::Quit => quit_app(app),
        Cli::Autostart => {}
    }
}

// ------------------------------------------------------------------------------------------------ setup

/// A thread that saves the panel position it is sent, once no new one has come for [`POS_SETTLE`]: a drag sends
/// one per pixel, the settings file is written once.
fn position_saver(core: Arc<Core>) -> mpsc::Sender<(i32, i32)> {
    let (tx, rx) = mpsc::channel::<(i32, i32)>();
    let spawned = std::thread::Builder::new().name("panel-position".into()).spawn(move || {
        while let Ok(mut pos) = rx.recv() {
            while let Ok(next) = rx.recv_timeout(POS_SETTLE) {
                pos = next;
            }
            core.save_panel_pos(pos);
        }
    });
    if let Err(e) = spawned {
        crate::core::log(&format!("panel position saver: {e}"));
    }
    tx
}

fn build_tray(app: &AppHandle, core: &Arc<Core>, panel: &Arc<Panel>) -> tauri::Result<TrayItems> {
    let extend = MenuItem::with_id(app, "extend", "Extend to TV", true, None::<&str>)?;
    let mirror = MenuItem::with_id(app, "mirror", "Mirror screen", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "Stop", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open panel", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start with Windows",
        true,
        platform::autostart_enabled(),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(app, &[&extend, &mirror, &stop, &sep()?, &open, &autostart, &sep()?, &quit])?;

    let icon = tray_icon(&core.state()).unwrap_or_else(|| app.default_window_icon().cloned().expect("app icon"));
    let (core_menu, panel_menu) = (Arc::clone(core), Arc::clone(panel));
    let panel_click = Arc::clone(panel);
    TrayIconBuilder::with_id(TRAY)
        .icon(icon)
        .tooltip("TunnelRez")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, ev| match ev.id().as_ref() {
            "extend" => core_menu.start(Mode::Extend),
            "mirror" => core_menu.start(Mode::Mirror),
            "stop" => core_menu.stop(),
            "open" => panel_menu.show(app),
            "autostart" => {
                if let Ok(exe) = std::env::current_exe() {
                    let want = Autostart::from(!platform::autostart_enabled());
                    let _ = platform::set_autostart(&exe, want);
                }
                let _ = app.emit_to(PANEL, "snapshot", core_menu.snapshot());
            }
            "quit" => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(move |tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left, button_state: MouseButtonState::Up, rect, ..
            } = ev
            {
                let pos = rect.position.to_physical::<i32>(1.0);
                let size = rect.size.to_physical::<i32>(1.0);
                *panel_click.anchor.lock().expect("anchor lock poisoned") =
                    Some(Rect { left: pos.x, top: pos.y, right: pos.x + size.width, bottom: pos.y + size.height });
                panel_click.toggle(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(TrayItems { extend, mirror, stop, autostart })
}

#[expect(clippy::too_many_lines, reason = "the app's one list of commands and its setup, read top to bottom")]
fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--vdd-guard") {
        let pid = argv.get(2).and_then(|p| p.parse().ok());
        let index = argv.get(3).and_then(|i| i.parse().ok());
        if let (Some(pid), Some(index)) = (pid, index) {
            platform::run_vdd_guard(pid, index);
        }
        return;
    }
    let cli = parse_cli(&argv);
    let panel = Arc::new(Panel::default());
    let items: Arc<OnceLock<TrayItems>> = Arc::new(OnceLock::new());

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init({
            let panel = Arc::clone(&panel);
            move |app, args, _cwd| run_cli(app, &panel, parse_cli(&args))
        }))
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            start,
            stop,
            set_setting,
            finish_setup,
            test_tv,
            open_tv_app,
            open_store,
            set_autostart,
            refresh_monitors,
            fit_panel,
            hide_panel,
            set_keep_open,
            set_logging,
            set_test,
            add_to_runbook,
            open_logs,
            install_tv_speaker,
            fix_tv_monitor,
            refresh_audio,
            ui_ready,
            quit
        ])
        .setup({
            let (panel, items) = (Arc::clone(&panel), Arc::clone(&items));
            move |app| {
                let handle = app.handle().clone();
                if let Some(note) = settings::Settings::migration_note() {
                    crate::core::log(note);
                }
                platform::migrate_autostart();
                let ui = TauriUi {
                    app: handle.clone(),
                    items: Arc::clone(&items),
                    panel: Arc::clone(&panel),
                    icon_kind: Arc::new(Mutex::new(None)),
                    test_loaded: Arc::new(AtomicBool::new(false)),
                    ui_thread: std::thread::current().id(),
                };
                let core = Core::new(ui);
                app.manage(Arc::clone(&core));
                app.manage(Arc::clone(&panel));
                app.manage(Arc::clone(&items));
                let tray = build_tray(&handle, &core, &panel)?;
                let _ = items.set(tray);
                core.panel_hidden();
                *panel.show_when_ready.lock().expect("show lock poisoned") =
                    cli == Cli::Show || matches!(core.state(), State::FirstRun);
                if matches!(cli, Cli::Start(_) | Cli::Stop | Cli::Quit) {
                    run_cli(&handle, &panel, cli);
                }

                if let Some(w) = Panel::window(&handle) {
                    platform::webview_memory(&w, WebviewMemory::Low);
                    let _ = w.set_always_on_top(!core.keep_panel_open());
                    let moved = position_saver(Arc::clone(&core));
                    let (panel, handle2, w2) = (Arc::clone(&panel), handle.clone(), w.clone());
                    w.on_window_event(move |ev| match ev {
                        WindowEvent::Focused(false) => panel.blurred(&handle2),
                        // Dragged (or pulled back on screen) while kept open: remember it once it settles.
                        WindowEvent::Moved(pos) if keep_open(&handle2) && w2.is_visible().unwrap_or(false) => {
                            let _ = moved.send((pos.x, pos.y));
                        }
                        WindowEvent::CloseRequested { api, .. } => {
                            api.prevent_close();
                            panel.hide(&handle2);
                        }
                        _ => {}
                    });
                }
                let on_end = Arc::clone(&core);
                platform::on_session_end(move || on_end.shutdown())?;
                Ok(())
            }
        })
        .build(tauri::generate_context!())
        .expect("build the Tauri app");

    app.run(|app, ev| match ev {
        // Any exit (also one not started by us) first tears down off the event thread; see `quit_app`.
        RunEvent::ExitRequested { api, .. } if !SHUT_DOWN.load(Ordering::SeqCst) => {
            api.prevent_exit();
            quit_app(app);
        }
        RunEvent::Exit if !SHUT_DOWN.load(Ordering::SeqCst) => {
            // Last resort (an exit that could not be prevented): the event loop is ending anyway.
            if let Some(core) = app.try_state::<Arc<Core>>() {
                core.shutdown();
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        std::iter::once("tunnelrez.exe").chain(a.iter().copied()).map(String::from).collect()
    }

    #[test]
    fn cli() {
        assert_eq!(parse_cli(&args(&[])), Cli::Show);
        assert_eq!(parse_cli(&args(&["--extend"])), Cli::Start(Mode::Extend));
        assert_eq!(parse_cli(&args(&["--mirror"])), Cli::Start(Mode::Mirror));
        assert_eq!(parse_cli(&args(&["--stop"])), Cli::Stop);
        assert_eq!(parse_cli(&args(&["--autostart"])), Cli::Autostart);
        assert_eq!(parse_cli(&args(&["--bogus"])), Cli::Show);
    }

    #[test]
    fn kept_open_panel_stays_on_screen() {
        // A 1920×1080 monitor with a 48 px bottom taskbar, and one to its left.
        let work = Rect { left: 0, top: 0, right: 1920, bottom: 1032 };
        let left = Rect { left: -2560, top: 0, right: 0, bottom: 1600 };
        assert_eq!(clamp_into(work, 100, 200, 360, 600), (100, 200), "inside: unchanged");
        assert_eq!(clamp_into(work, 1800, 900, 360, 600), (1560, 432), "pulled back above the taskbar");
        assert_eq!(clamp_into(work, -500, -40, 360, 600), (0, 0));
        assert_eq!(clamp_into(left, -2700, 1500, 360, 600), (-2560, 1000), "negative coordinates");
        assert_eq!(clamp_into(work, 50, 50, 360, 2000), (50, 0), "too tall: the top stays visible");
    }
}
