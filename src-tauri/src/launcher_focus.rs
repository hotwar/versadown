//! Remembers whether the manager was the foreground window when the user
//! started interacting with the floating launcher.
//!
//! Pressing the launcher activates it before its DOM double-click handler
//! runs, so by then the manager has always lost focus. Without this history a
//! double click could not tell "the user is looking at the manager" (hide it)
//! from "the manager is buried under another application" (raise it).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Focus-out of one window and focus-in of the next are delivered as a pair.
/// Anything further apart is an unrelated focus change.
const FOCUS_HANDOFF: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct ManagerFocusHistory(Mutex<FocusState>);

#[derive(Default)]
struct FocusState {
    manager_blurred_at: Option<Instant>,
    launcher_focused_at: Option<Instant>,
    manager_was_foreground: bool,
}

fn within_handoff(earlier: Option<Instant>, now: Instant) -> bool {
    earlier.is_some_and(|at| now.saturating_duration_since(at) <= FOCUS_HANDOFF)
}

impl ManagerFocusHistory {
    pub fn manager_focus_changed(&self, focused: bool, now: Instant) {
        let Ok(mut state) = self.0.lock() else { return };
        if focused {
            state.manager_blurred_at = None;
            state.manager_was_foreground = false;
        } else {
            state.manager_blurred_at = Some(now);
            // The window manager may report the launcher's focus-in first.
            if within_handoff(state.launcher_focused_at, now) {
                state.manager_was_foreground = true;
            }
        }
    }

    pub fn launcher_focus_changed(&self, focused: bool, now: Instant) {
        let Ok(mut state) = self.0.lock() else { return };
        if focused {
            state.launcher_focused_at = Some(now);
            state.manager_was_foreground = within_handoff(state.manager_blurred_at, now);
        } else {
            state.launcher_focused_at = None;
        }
    }

    /// True when focus moved straight from the manager to the launcher.
    pub fn manager_was_foreground(&self) -> bool {
        self.0.lock().is_ok_and(|state| state.manager_was_foreground)
    }
}

/// A launcher double click hides the manager only while the user is looking at
/// it; a hidden, minimized or buried manager is brought to the front instead.
pub fn launcher_toggle_hides_manager(
    visible: bool,
    minimized: bool,
    focused: bool,
    was_foreground_before_launcher: bool,
) -> bool {
    visible && !minimized && (focused || was_foreground_before_launcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn focus_moving_straight_from_manager_to_launcher_marks_the_manager_foreground() {
        let start = Instant::now();
        let history = ManagerFocusHistory::default();
        history.manager_focus_changed(true, start);
        history.manager_focus_changed(false, at(start, 1_000));
        history.launcher_focus_changed(true, at(start, 1_010));
        assert!(history.manager_was_foreground());
    }

    #[test]
    fn launcher_focus_reported_before_the_manager_blur_is_still_a_handoff() {
        let start = Instant::now();
        let history = ManagerFocusHistory::default();
        history.launcher_focus_changed(true, at(start, 1_000));
        history.manager_focus_changed(false, at(start, 1_005));
        assert!(history.manager_was_foreground());
    }

    #[test]
    fn focus_arriving_from_another_application_leaves_the_manager_buried() {
        let start = Instant::now();
        let history = ManagerFocusHistory::default();
        // Manager -> another app, then much later the user presses the launcher.
        history.manager_focus_changed(false, start);
        history.launcher_focus_changed(true, at(start, 3_000));
        assert!(!history.manager_was_foreground());
    }

    #[test]
    fn a_second_launcher_interaction_from_elsewhere_resets_the_previous_handoff() {
        let start = Instant::now();
        let history = ManagerFocusHistory::default();
        history.manager_focus_changed(false, start);
        history.launcher_focus_changed(true, at(start, 5));
        assert!(history.manager_was_foreground());
        // The user moves to another application, then back to the launcher.
        history.launcher_focus_changed(false, at(start, 2_000));
        history.launcher_focus_changed(true, at(start, 6_000));
        assert!(!history.manager_was_foreground());
    }

    #[test]
    fn regaining_focus_clears_the_handoff() {
        let start = Instant::now();
        let history = ManagerFocusHistory::default();
        history.manager_focus_changed(false, start);
        history.launcher_focus_changed(true, at(start, 5));
        history.manager_focus_changed(true, at(start, 1_000));
        assert!(!history.manager_was_foreground());
    }

    #[test]
    fn double_click_hides_only_a_manager_the_user_was_looking_at() {
        // Foreground manager: focus already moved to the launcher.
        assert!(launcher_toggle_hides_manager(true, false, false, true));
        // Still focused (platforms where the launcher does not take focus).
        assert!(launcher_toggle_hides_manager(true, false, true, false));
        // Buried under another application: raise it instead.
        assert!(!launcher_toggle_hides_manager(true, false, false, false));
        // Minimized or hidden: always reveal.
        assert!(!launcher_toggle_hides_manager(true, true, false, true));
        assert!(!launcher_toggle_hides_manager(false, false, false, true));
    }
}
