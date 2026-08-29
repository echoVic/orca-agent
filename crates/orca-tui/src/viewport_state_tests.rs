//! Viewport owner tests: scrolling, pinning, and post-completion input.

use crossbeam_channel as mpsc;

use crate::protocol::TuiEvent;
use crate::transcript_state::ChatMessage;
use crate::types::AppState;

fn state() -> AppState {
    let (tx, _rx) = mpsc::unbounded();
    AppState::new(
        tx,
        "0.0.0-test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    )
}

#[test]
fn session_completion_re_pins_to_bottom_after_incidental_scroll() {
    let mut state = state();
    state.enter_running();
    state.viewport.total_lines = 100;
    state.viewport.visible_height = 20;
    state.viewport.scroll_offset = 60;
    state.viewport.auto_scroll = false;
    state
        .transcript
        .messages
        .push(ChatMessage::Assistant("final answer".to_string()));

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    assert!(
        state.viewport.auto_scroll,
        "finished turns should leave the final answer pinned above the composer"
    );
    assert_eq!(state.viewport.scroll_offset, 80);
}

#[test]
fn scroll_up_with_content_shorter_than_pane_keeps_auto_follow() {
    // First screen: everything fits, nothing to scroll. A stray wheel-up (trackpad
    // inertia, accidental touch) must not disarm auto-follow, or the transcript
    // stops tracking new streamed content once it grows past one screen and the
    // user is forced to scroll down by hand.
    let mut state = state();
    state.viewport.total_lines = 10;
    state.viewport.visible_height = 24;
    state.viewport.auto_scroll = true;

    state.scroll_up(3);

    assert!(
        state.viewport.auto_scroll,
        "wheel-up on a not-yet-overflowing transcript must keep auto-follow armed"
    );
    assert_eq!(state.viewport.scroll_offset, 0);
}

#[test]
fn scroll_up_with_overflow_disarms_auto_follow() {
    let mut state = state();
    state.viewport.total_lines = 100;
    state.viewport.visible_height = 24;
    state.viewport.scroll_offset = 76;
    state.viewport.auto_scroll = true;

    state.scroll_up(3);

    assert!(
        !state.viewport.auto_scroll,
        "wheel-up on an overflowing transcript should still let the user break away"
    );
    assert_eq!(state.viewport.scroll_offset, 73);
}

#[test]
fn scroll_navigation_preserves_offsets_above_u16_max() {
    let mut state = state();
    state.viewport.total_lines = 100_000;
    state.viewport.visible_height = 20;
    state.viewport.scroll_offset = 70_000;
    state.viewport.auto_scroll = false;

    state.scroll_down(5_000usize);
    assert_eq!(state.viewport.scroll_offset, 75_000);
    state.scroll_up(10_000usize);
    assert_eq!(state.viewport.scroll_offset, 65_000);
}

#[test]
fn scroll_down_saturates_when_total_height_reaches_usize_max() {
    let mut state = state();
    state.viewport.total_lines = usize::MAX;
    state.viewport.visible_height = 0;
    state.viewport.scroll_offset = usize::MAX - 1;
    state.viewport.auto_scroll = false;

    state.scroll_down(10usize);

    assert_eq!(state.viewport.scroll_offset, usize::MAX);
    assert!(state.viewport.auto_scroll);
}

#[test]
fn session_completion_temporarily_ignores_inertial_mouse_scroll() {
    let mut state = state();
    state.enter_running();
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    let completed_at = state
        .last_completed_at
        .expect("session completion should record completion time");

    assert!(
        !state.accepts_mouse_scroll_at(completed_at),
        "trackpad inertia immediately after completion must not undo bottom pinning"
    );
    assert!(
        state.accepts_mouse_scroll_at(completed_at + std::time::Duration::from_millis(900)),
        "manual mouse scrolling should work again after the completion grace period"
    );
}
