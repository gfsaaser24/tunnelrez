//! The app's state machine: `FirstRun`, `Idle`, `Connecting`, `Streaming`, `Stopping`, `Error(kind)`.
//!
//! `State::on` is a pure transition function, so every rule about what may follow what lives here and is tested
//! here; the controller only feeds it events.

use serde::Serialize;

use crate::settings::Mode;

#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum State {
    /// No TV address yet.
    #[default]
    FirstRun,
    Idle,
    Connecting {
        mode: Mode,
        phase: Phase,
    },
    Streaming {
        mode: Mode,
    },
    /// Sending BYE, waiting for the sender to exit, unplugging the monitor.
    Stopping,
    Error {
        error: Failure,
    },
}

/// What a connecting stream is doing right now (shown under the title).
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Checking that the TV answers on its port.
    Checking,
    /// Plugging the virtual monitor.
    Plugging,
    /// The sender is starting (probing encoders, starting capture).
    Starting,
    /// The stream dropped; the sender is reconnecting.
    Reconnecting,
    /// The sender exited; it is restarted after a short wait.
    Restarting,
}

/// Why the app is in the Error state. Each has its own screen in the panel.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub(crate) enum Failure {
    /// The TV did not answer at all (off, asleep, wrong address, other network).
    TvUnreachable { host: String },
    /// The TV answers, but the `TunnelRez` app on it is not listening.
    TvAppNotOpen { host: String },
    /// No sender executable was found.
    SenderMissing,
    /// The virtual monitor could not be plugged. `cause` says why in the panel's words; `message` is the detail for
    /// the logs (a raw driver message, too long and too technical to show).
    MonitorFailed { cause: MonitorCause, message: String },
    /// The sender could not switch the (reused) virtual monitor to the picked size. Retrying the same size does
    /// not help, so the sender is not restarted.
    MonitorModeFailed { width: u32, height: u32 },
    /// The sender kept failing and the app gave up.
    SenderFailed { message: String },
    /// The picked encoder x codec cannot run on this PC (the sender's `caps` probe), or Windows has no HEVC codec
    /// for it (`hevc_store`: show the Microsoft Store buttons).
    Unsupported { message: String, hevc_store: bool },
}

/// Why the virtual monitor failed, as far as `sender vdd status` can tell. Only [`MonitorCause::Missing`] may say the
/// driver is missing.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MonitorCause {
    /// The Parsec driver is not installed.
    Missing,
    /// Windows must restart to finish its install.
    RestartWindows,
    /// The adapter is off in Device Manager.
    Disabled,
    /// The adapter or a monitor has a Device Manager problem.
    AdapterProblem,
    /// The driver looks fine, and Windows still did not show the monitor.
    NotAttached,
    /// This sender cannot say.
    Unknown,
}

/// Inputs to the state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The first-run screen saved a TV address.
    SetupDone,
    Start(Mode),
    Phase(Phase),
    /// Frames are flowing to the TV.
    Connected,
    /// The connection dropped; the sender is trying again.
    Lost(Phase),
    Failed(Failure),
    Stop,
    /// Everything has stopped and been cleaned up.
    Stopped,
}

impl State {
    /// The state after `event`, or `None` when the event does not apply in this state (it is then ignored).
    #[must_use]
    pub(crate) fn on(&self, event: Event) -> Option<State> {
        use State::*;
        match (self, event) {
            (FirstRun | Idle | Error { .. }, Event::Start(mode)) => Some(Connecting { mode, phase: Phase::Checking }),
            (Connecting { mode, .. }, Event::Phase(phase))
            | (Streaming { mode } | Connecting { mode, .. }, Event::Lost(phase)) => {
                Some(Connecting { mode: *mode, phase })
            }
            (Connecting { mode, .. }, Event::Connected) => Some(Streaming { mode: *mode }),
            (Connecting { .. } | Streaming { .. }, Event::Failed(error)) => Some(Error { error }),
            (Connecting { .. } | Streaming { .. } | Error { .. }, Event::Stop) => Some(Stopping),
            (FirstRun, Event::SetupDone) | (Stopping, Event::Stopped) => Some(Idle),
            _ => None,
        }
    }

    /// A stream is running or being set up (the primary button is Stop).
    pub(crate) fn is_active(&self) -> bool {
        matches!(self, State::Connecting { .. } | State::Streaming { .. })
    }

    /// Short words for the tray tooltip.
    pub(crate) fn summary(&self) -> &'static str {
        match self {
            State::FirstRun => "set up",
            State::Idle => "ready",
            State::Connecting { phase: Phase::Reconnecting, .. } => "reconnecting",
            State::Connecting { .. } => "connecting",
            State::Streaming { .. } => "streaming",
            State::Stopping => "stopping",
            State::Error { error } => match error {
                Failure::TvUnreachable { .. } => "can't reach the TV",
                Failure::TvAppNotOpen { .. } => "TV app not open",
                Failure::SenderMissing => "sender not found",
                Failure::MonitorFailed { .. } => "virtual monitor failed",
                Failure::MonitorModeFailed { .. } => "could not switch the TV monitor",
                Failure::SenderFailed { .. } => "stream failed",
                Failure::Unsupported { .. } => "encoder can't run this",
            },
        }
    }
}

/// A monitor size in plain words: `4K` for 3840×2160, else `2560x1440`.
pub(crate) fn size_words(width: u32, height: u32) -> String {
    if (width, height) == (3840, 2160) {
        "4K".to_string()
    } else {
        format!("{width}x{height}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connecting() -> State {
        State::Connecting { mode: Mode::Extend, phase: Phase::Checking }
    }

    #[test]
    fn happy_path() {
        let s = State::FirstRun.on(Event::SetupDone).unwrap();
        assert_eq!(s, State::Idle);
        let s = s.on(Event::Start(Mode::Extend)).unwrap();
        assert_eq!(s, connecting());
        let s = s.on(Event::Phase(Phase::Starting)).unwrap();
        let s = s.on(Event::Connected).unwrap();
        assert_eq!(s, State::Streaming { mode: Mode::Extend });
        let s = s.on(Event::Stop).unwrap();
        assert_eq!(s, State::Stopping);
        assert_eq!(s.on(Event::Stopped), Some(State::Idle));
    }

    #[test]
    fn reconnect_keeps_mode() {
        let s = State::Streaming { mode: Mode::Mirror };
        assert_eq!(
            s.on(Event::Lost(Phase::Reconnecting)),
            Some(State::Connecting { mode: Mode::Mirror, phase: Phase::Reconnecting })
        );
    }

    #[test]
    fn errors_and_retry() {
        let e = connecting().on(Event::Failed(Failure::TvAppNotOpen { host: "tv".into() })).unwrap();
        assert!(matches!(e, State::Error { error: Failure::TvAppNotOpen { .. } }));
        assert!(!e.is_active());
        assert_eq!(e.on(Event::Start(Mode::Extend)), Some(connecting()));
        assert_eq!(e.on(Event::Stop), Some(State::Stopping));
    }

    #[test]
    fn stray_events_are_ignored() {
        assert_eq!(State::Idle.on(Event::Connected), None);
        assert_eq!(State::Idle.on(Event::Stop), None);
        assert_eq!(State::Stopping.on(Event::Start(Mode::Extend)), None);
        assert_eq!(State::Stopping.on(Event::Connected), None);
        assert_eq!(State::Streaming { mode: Mode::Extend }.on(Event::Start(Mode::Mirror)), None);
        assert_eq!(State::Idle.on(Event::Failed(Failure::SenderMissing)), None);
    }

    #[test]
    fn mode_failure_words() {
        assert_eq!(size_words(3840, 2160), "4K");
        assert_eq!(size_words(2560, 1440), "2560x1440");
        let cause = Failure::MonitorFailed { cause: MonitorCause::RestartWindows, message: "detail".into() };
        let json = serde_json::to_value(&cause).unwrap();
        assert_eq!((json["code"].as_str(), json["cause"].as_str()), (Some("monitor_failed"), Some("restart_windows")));
        let e = connecting().on(Event::Failed(Failure::MonitorModeFailed { width: 3840, height: 2160 })).unwrap();
        assert!(!e.is_active());
        assert_eq!(e.summary(), "could not switch the TV monitor");
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["error"]["code"], "monitor_mode_failed");
        assert_eq!(json["error"]["width"], 3840);
    }
}
