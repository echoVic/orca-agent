use crossbeam_channel as mpsc;

use crate::shortcuts::RunningShortcut;
use crate::types::{AppState, AppStatus, UserAction};

pub(crate) fn handle_running_shortcut(
    shortcut: RunningShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    match shortcut {
        RunningShortcut::BackgroundCurrentTurn => {
            let _ = action_tx.send(UserAction::BackgroundCurrentTurn);
            state.request_runtime_queue_start();
            state.set_status(AppStatus::Idle);
            state.resume_queued_follow_up_autosend();
        }
        RunningShortcut::Interrupt => {
            let _ = action_tx.send(UserAction::Interrupt);
            state.request_runtime_queue_pause();
            state.suspend_queued_follow_up_autosend();
        }
        RunningShortcut::ScrollUp => {
            state.scroll_up(1);
        }
        RunningShortcut::ScrollDown => {
            state.scroll_down(1);
        }
        RunningShortcut::PageUp => {
            let page = state.visible_height.saturating_sub(2);
            state.scroll_up(page);
        }
        RunningShortcut::PageDown => {
            let page = state.visible_height.saturating_sub(2);
            state.scroll_down(page);
        }
        RunningShortcut::HalfPageUp => {
            let page = state.visible_height / 2;
            state.scroll_up(page);
        }
        RunningShortcut::HalfPageDown => {
            let page = state.visible_height / 2;
            state.scroll_down(page);
        }
        RunningShortcut::SubmitQueued
        | RunningShortcut::Newline
        | RunningShortcut::EditLatestQueued => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queued_input::QueuedUserMessage;
    use orca_runtime::mentions::MentionBindings;

    fn state(action_tx: mpsc::Sender<UserAction>) -> AppState {
        AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    fn queued(text: &str) -> QueuedUserMessage {
        QueuedUserMessage::from_composer(text.to_string(), Vec::new(), MentionBindings::default())
            .unwrap()
    }

    #[test]
    fn running_interrupt_suspends_queued_autosend() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state(action_tx.clone());
        state.enter_running();

        handle_running_shortcut(RunningShortcut::Interrupt, &mut state, &action_tx);

        assert!(!state.queued_autosend_enabled());
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    #[ignore = "runtime actor owns queued submission ordering"]
    fn background_control_precedes_one_queued_submit() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state(action_tx.clone());
        state.enter_running();
        state.enqueue_user_message(queued("follow up")).unwrap();

        handle_running_shortcut(
            RunningShortcut::BackgroundCurrentTurn,
            &mut state,
            &action_tx,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::BackgroundCurrentTurn)
        ));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == "follow up"
        ));
        assert!(state.queued_submission_in_flight());
    }
}
