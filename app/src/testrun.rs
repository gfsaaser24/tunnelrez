//! The built-in test run (the panel's Test button): while streaming in Extend mode, the moving test page
//! (`ui/test/motion60.html`, drawn at most 60 times a second) is shown full screen on the streamed virtual
//! monitor, never on another screen. Once the window really fills that monitor, the Log starts; the page closes
//! when the log ends or the stream stops, and it is fitted again when the monitor changes size.
//!
//! Lessons from the external page watcher used for the first real-TV batch: at 4K a browser window can take
//! several seconds to appear, so the run waits for it and never relaunches in a loop (it only moves the window it
//! has); the page is an app-owned borderless window, so no browser profile can be left locked.
//!
//! This module decides; `core.rs` reads the inputs once per [`TICK`] and carries out the [`Action`].

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::platform::Rect;
use crate::settings::Mode;
use crate::state::State;

/// What the run book's `content` says for a run on the test page.
pub(crate) const CONTENT: &str = "motion60 test page";
/// The page, relative to the panel's files (`ui/`).
pub(crate) const PAGE: &str = "test/motion60.html";
/// How often the run looks at the window.
pub(crate) const TICK: Duration = Duration::from_millis(200);
/// The page must fill the monitor this long before the log starts (the first frames are drawn).
pub(crate) const SETTLE: Duration = Duration::from_secs(1);
/// A window that loaded but did not land on the monitor is moved again after this long (never sooner).
pub(crate) const REPLACE_AFTER: Duration = Duration::from_millis(1500);
/// The page must fill the monitor within this long while streaming, or the run fails.
pub(crate) const FIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The panel's words when Test cannot start.
/// (Short: it fits the row. The page opens only on the TV screen, never on a screen of this PC.)
pub(crate) const NEEDS_EXTEND: &str = "Needs Extend (TV screen only)";
pub(crate) const NEEDS_STREAM: &str = "Start the stream first";
pub(crate) const NO_SCREEN: &str =
    "No TV screen the page may cover (the virtual monitor is missing or is the main screen).";

/// The test run as the panel shows it.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TestState {
    #[default]
    Off,
    /// Opening the page, or waiting until it fills the TV screen.
    Opening,
    /// The page fills the TV screen and the log records.
    Running,
    /// The last run could not go on.
    Failed { message: String },
}

impl TestState {
    pub(crate) fn is_on(&self) -> bool {
        matches!(self, Self::Opening | Self::Running)
    }
}

/// The Test button: the run's state, and why it cannot start now (`None` = it can).
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TestView {
    pub(crate) state: TestState,
    pub(crate) blocked: Option<&'static str>,
}

/// Why a test run cannot start in `state` / `mode`.
pub(crate) fn blocked(state: &State, mode: Mode) -> Option<&'static str> {
    if mode != Mode::Extend {
        Some(NEEDS_EXTEND)
    } else if !matches!(state, State::Streaming { .. }) {
        Some(NEEDS_STREAM)
    } else {
        None
    }
}

/// The test page window: where it is (physical px) and whether its page has loaded and it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TestPage {
    pub(crate) rect: Rect,
    pub(crate) loaded: bool,
}

/// Where the stream is, for the test run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    /// Not running (stopped, or failed).
    Stopped,
    /// Being restarted on purpose (a setting changed): not a stop.
    Restarting,
    /// Starting or reconnecting.
    Connecting,
    /// Frames reach the TV.
    Streaming,
}

impl Stream {
    /// From the app's state; the controller says [`Stream::Restarting`] itself during a restart on purpose.
    pub(crate) fn of(state: &State) -> Self {
        match state {
            State::Streaming { .. } => Self::Streaming,
            State::Connecting { .. } => Self::Connecting,
            _ => Self::Stopped,
        }
    }
}

/// What the run reads each tick.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inputs {
    pub(crate) stream: Stream,
    pub(crate) mode: Mode,
    /// The streamed virtual monitor, when it is up and safe to cover (see `platform::test_screen`).
    pub(crate) screen: Option<Rect>,
    pub(crate) page: Option<TestPage>,
    /// A log is recording.
    pub(crate) log_running: bool,
    pub(crate) now: Instant,
}

/// What to do with the page this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Nothing,
    /// Close the page (the TV screen may come and go).
    Close,
    /// Open the page, or move the open one, to cover this rectangle.
    Place(Rect),
    /// The page fills the TV screen: start the log.
    StartLog,
}

/// Why a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Test pressed again.
    Stopped,
    /// The stream stopped (Stop, or a failure).
    StreamStopped,
    /// The log ended (its time limit, or switched off).
    LogEnded,
    /// Someone closed the page.
    Closed,
    Failed(String),
}

/// One tick's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tick {
    pub(crate) action: Action,
    /// The page fills the TV screen now: the log's `content` is the test page.
    pub(crate) fits: bool,
    pub(crate) end: Option<End>,
}

impl Tick {
    /// The page fills the TV screen.
    fn fits(action: Action) -> Self {
        Self { action, fits: true, end: None }
    }

    /// It does not (yet).
    fn waits(action: Action) -> Self {
        Self { action, fits: false, end: None }
    }

    fn end(end: End) -> Self {
        Self { action: Action::Nothing, fits: false, end: Some(end) }
    }
}

/// One test run's memory between ticks.
#[derive(Debug, Default)]
pub(crate) struct Run {
    /// Where the page was last put, and when.
    placed: Option<(Rect, Instant)>,
    /// The page window has existed since it was last placed.
    seen: bool,
    fitted_since: Option<Instant>,
    /// Streaming, but the page does not fill the screen, since then.
    waiting_since: Option<Instant>,
    log_started: bool,
}

impl Run {
    /// This run started the log.
    pub(crate) fn log_started(&self) -> bool {
        self.log_started
    }

    /// The [`Action::Place`] of the last tick was not carried out (the stream stopped or restarted after the tick
    /// read it): forget it, so the next tick on a running stream places the page again.
    pub(crate) fn placement_skipped(&mut self) {
        self.placed = None;
    }

    pub(crate) fn tick(&mut self, i: &Inputs) -> Tick {
        if i.stream == Stream::Stopped {
            return Tick::end(End::StreamStopped);
        }
        if i.mode != Mode::Extend {
            return Tick::end(End::Failed(NEEDS_EXTEND.into()));
        }
        if self.log_started && !i.log_running {
            return Tick::end(End::LogEnded);
        }
        let Some(screen) = i.screen.filter(|_| i.stream == Stream::Streaming) else {
            // Restarting or reconnecting: no page while the TV screen may disappear (Windows would move it to
            // another screen).
            self.fitted_since = None;
            self.seen = false;
            let open = self.placed.take().is_some() || i.page.is_some();
            if i.stream == Stream::Streaming {
                // Streaming, but no screen the page may cover (the virtual monitor is the main screen, or gone):
                // that does not get better by waiting.
                let waiting = *self.waiting_since.get_or_insert(i.now);
                if i.now.duration_since(waiting) >= FIT_TIMEOUT {
                    return Tick::end(End::Failed(NO_SCREEN.into()));
                }
            } else {
                self.waiting_since = None;
            }
            return Tick::waits(if open { Action::Close } else { Action::Nothing });
        };
        match i.page {
            Some(_) => self.seen = true,
            None if self.seen => return Tick::end(End::Closed),
            None => {}
        }
        let fits = i.page.is_some_and(|p| p.loaded && p.rect == screen);
        if !fits {
            self.fitted_since = None;
            let waiting = *self.waiting_since.get_or_insert(i.now);
            if i.now.duration_since(waiting) >= FIT_TIMEOUT {
                return Tick::end(End::Failed(format!(
                    "The page did not fill the TV screen in {} s.",
                    FIT_TIMEOUT.as_secs()
                )));
            }
            // Open it once; move it again only when the screen changed, or when it loaded somewhere else.
            let due = match self.placed {
                None => true,
                Some((r, at)) => {
                    r != screen || (i.page.is_some_and(|p| p.loaded) && i.now.duration_since(at) >= REPLACE_AFTER)
                }
            };
            if due {
                self.placed = Some((screen, i.now));
                return Tick::waits(Action::Place(screen));
            }
            return Tick::waits(Action::Nothing);
        }
        self.waiting_since = None;
        let since = *self.fitted_since.get_or_insert(i.now);
        if !self.log_started && i.now.duration_since(since) >= SETTLE {
            self.log_started = true;
            return Tick::fits(Action::StartLog);
        }
        Tick::fits(Action::Nothing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TV_1080: Rect = Rect { left: 2560, top: 0, right: 4480, bottom: 1080 };
    const TV_4K: Rect = Rect { left: 2560, top: 0, right: 6400, bottom: 2160 };

    fn inputs(now: Instant) -> Inputs {
        Inputs {
            stream: Stream::Streaming,
            mode: Mode::Extend,
            screen: Some(TV_1080),
            page: None,
            log_running: false,
            now,
        }
    }

    /// The window is at `rect` and its page has loaded.
    fn page(rect: Rect) -> TestPage {
        TestPage { rect, loaded: true }
    }

    /// The window is at `rect`, still loading.
    fn loading(rect: Rect) -> TestPage {
        TestPage { rect, loaded: false }
    }

    #[test]
    fn opens_once_waits_for_the_fit_then_starts_the_log() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut r = Run::default();
        assert_eq!(r.tick(&inputs(t0)).action, Action::Place(TV_1080));
        // Slow to appear at 4K: no relaunch while it is not there, nor while it is loading.
        assert_eq!(r.tick(&inputs(at(200))).action, Action::Nothing);
        assert_eq!(r.tick(&Inputs { page: Some(loading(TV_1080)), ..inputs(at(5000)) }).action, Action::Nothing);
        // It fills the screen: the log starts after the settle time, once.
        let fit = |ms| Inputs { page: Some(page(TV_1080)), ..inputs(at(ms)) };
        assert_eq!(r.tick(&fit(6000)), Tick::fits(Action::Nothing));
        assert_eq!(r.tick(&fit(7000)).action, Action::StartLog);
        assert!(r.log_started());
        let running = |ms| Inputs { log_running: true, ..fit(ms) };
        assert_eq!(r.tick(&running(7200)).action, Action::Nothing);
        // The log ended (2 minutes): the run ends.
        assert_eq!(r.tick(&fit(130_000)).end, Some(End::LogEnded));
    }

    #[test]
    fn a_window_that_landed_wrong_is_moved_not_relaunched() {
        let t0 = Instant::now();
        let mut r = Run::default();
        assert_eq!(r.tick(&inputs(t0)).action, Action::Place(TV_1080));
        let wrong = Rect { left: 2560, top: 0, right: 3520, bottom: 540 };
        let i = |ms| Inputs { page: Some(page(wrong)), ..inputs(t0 + Duration::from_millis(ms)) };
        assert_eq!(r.tick(&i(1000)).action, Action::Nothing, "not before REPLACE_AFTER");
        assert_eq!(r.tick(&i(1600)).action, Action::Place(TV_1080));
        assert_eq!(r.tick(&i(1800)).action, Action::Nothing);
        assert!(matches!(r.tick(&i(31_000)).end, Some(End::Failed(_))), "never fits: gives up after 30 s");
    }

    #[test]
    fn refits_on_a_resolution_change_and_hides_during_a_restart() {
        let t0 = Instant::now();
        let mut r = Run::default();
        let _ = r.tick(&inputs(t0));
        let fit = Inputs { page: Some(page(TV_1080)), log_running: true, ..inputs(t0) };
        let _ = r.tick(&fit);
        // 1080p -> 4K: the stream restarts; the page closes while the screen may come and go.
        let restarting = Inputs { stream: Stream::Restarting, ..fit };
        assert_eq!(r.tick(&restarting).action, Action::Close);
        assert_eq!(r.tick(&Inputs { page: None, ..restarting }).action, Action::Nothing);
        // Streaming again at 4K: the page opens on the new size.
        let back = Inputs { screen: Some(TV_4K), page: None, ..fit };
        assert_eq!(r.tick(&back).action, Action::Place(TV_4K));
        // The same monitor changing size under an open page also moves it.
        let mut r = Run::default();
        let _ = r.tick(&inputs(t0));
        let _ = r.tick(&Inputs { page: Some(page(TV_1080)), ..inputs(t0) });
        assert_eq!(
            r.tick(&Inputs { screen: Some(TV_4K), page: Some(page(TV_1080)), ..inputs(t0) }).action,
            Action::Place(TV_4K)
        );
    }

    #[test]
    fn never_without_the_tv_screen() {
        let t0 = Instant::now();
        let mut r = Run::default();
        // No safe screen (the virtual monitor is the main one, or it is not up): nothing opens, and after the fit
        // timeout the run fails instead of opening forever (review on #24).
        assert_eq!(r.tick(&Inputs { screen: None, ..inputs(t0) }).action, Action::Nothing);
        let later = Inputs { screen: None, ..inputs(t0 + Duration::from_secs(31)) };
        assert_eq!(r.tick(&later).end, Some(End::Failed(NO_SCREEN.into())));
        // A reconnect in between restarts the wait.
        let mut r = Run::default();
        let _ = r.tick(&Inputs { screen: None, ..inputs(t0) });
        let _ = r.tick(&Inputs { stream: Stream::Connecting, ..inputs(t0 + Duration::from_secs(20)) });
        assert_eq!(r.tick(&Inputs { screen: None, ..inputs(t0 + Duration::from_secs(31)) }).end, None);
        assert_eq!(r.tick(&Inputs { mode: Mode::Mirror, ..inputs(t0) }).end, Some(End::Failed(NEEDS_EXTEND.into())));
        let mut r = Run::default();
        assert_eq!(r.tick(&Inputs { stream: Stream::Stopped, ..inputs(t0) }).end, Some(End::StreamStopped));
        // Reconnecting: the page waits (closed), the run goes on.
        let mut r = Run::default();
        assert_eq!(r.tick(&Inputs { stream: Stream::Connecting, ..inputs(t0) }), Tick::waits(Action::Nothing));
        // Closed by someone after it was open: the run ends (no reopen loop).
        let mut r = Run::default();
        let _ = r.tick(&inputs(t0));
        let _ = r.tick(&Inputs { page: Some(loading(TV_1080)), ..inputs(t0) });
        assert_eq!(r.tick(&inputs(t0)).end, Some(End::Closed));
    }

    #[test]
    fn a_skipped_placement_is_tried_again() {
        let t0 = Instant::now();
        let mut r = Run::default();
        assert_eq!(r.tick(&inputs(t0)).action, Action::Place(TV_1080));
        // The controller found the stream restarted under it and did not show the page (review on #24).
        r.placement_skipped();
        assert_eq!(r.tick(&inputs(t0 + Duration::from_millis(200))).action, Action::Place(TV_1080));
    }

    #[test]
    fn test_is_blocked_outside_an_extend_stream() {
        assert_eq!(blocked(&State::Idle, Mode::Extend), Some(NEEDS_STREAM));
        assert_eq!(blocked(&State::Streaming { mode: Mode::Mirror }, Mode::Mirror), Some(NEEDS_EXTEND));
        assert_eq!(blocked(&State::Streaming { mode: Mode::Extend }, Mode::Extend), None);
        assert_eq!(Stream::of(&State::Stopping), Stream::Stopped);
        assert_eq!(Stream::of(&State::Streaming { mode: Mode::Extend }), Stream::Streaming);
    }
}
