//! Platform-independent scheduling decisions for the macOS pointer poller.
use std::time::Duration;

pub(crate) const ACTIVE_INTERVAL: Duration = Duration::from_millis(30);
pub(crate) const IDLE_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PollStep {
    /// Window is gone or the app is quitting: leave the loop.
    Stop,
    /// Launcher hidden: sleep long and do not track the pointer.
    Idle(Duration),
    /// Launcher visible: track the pointer, then sleep briefly.
    Track(Duration),
}

/// `visible` is `None` when querying the window failed (it was destroyed).
pub(crate) fn next_step(exiting: bool, visible: Option<bool>) -> PollStep {
    match (exiting, visible) {
        (true, _) | (_, None) => PollStep::Stop,
        (false, Some(false)) => PollStep::Idle(IDLE_INTERVAL),
        (false, Some(true)) => PollStep::Track(ACTIVE_INTERVAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_when_exiting_or_window_invalid() {
        assert_eq!(next_step(true, Some(true)), PollStep::Stop);
        assert_eq!(next_step(false, None), PollStep::Stop);
    }

    #[test]
    fn hidden_window_polls_slowly() {
        assert_eq!(next_step(false, Some(false)), PollStep::Idle(IDLE_INTERVAL));
    }

    #[test]
    fn visible_window_polls_fast() {
        assert_eq!(next_step(false, Some(true)), PollStep::Track(ACTIVE_INTERVAL));
        assert!(IDLE_INTERVAL > ACTIVE_INTERVAL * 8);
    }
}
