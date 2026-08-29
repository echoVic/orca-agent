use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent};

use orca_core::approval_types::ApprovalMode;

use crate::protocol::UserAction;
use crate::types::AppState;

pub(crate) const IMPLEMENT_APPROVED_PLAN_PROMPT: &str = "Implement the approved plan.";

pub(crate) fn handle_plan_approval_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    match key.code {
        KeyCode::Up | KeyCode::Left => select(state, 0),
        KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            if let Some(dialog) = state.plan_approval_dialog.as_mut() {
                dialog.selected = (dialog.selected + 1) % 2;
            }
        }
        KeyCode::PageUp => state.scroll_up(state.viewport.visible_height.max(1)),
        KeyCode::PageDown => state.scroll_down(state.viewport.visible_height.max(1)),
        KeyCode::Char('1') => implement(state, action_tx),
        KeyCode::Char('2') | KeyCode::Esc => stay_in_plan_mode(state),
        KeyCode::Enter => {
            let selected = state
                .plan_approval_dialog
                .as_ref()
                .map_or(1, |dialog| dialog.selected);
            if selected == 0 {
                implement(state, action_tx);
            } else {
                stay_in_plan_mode(state);
            }
        }
        _ => {}
    }
}

fn select(state: &mut AppState, selected: usize) {
    if let Some(dialog) = state.plan_approval_dialog.as_mut() {
        dialog.selected = selected.min(1);
    }
}

fn implement(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>) {
    let target_mode = state
        .pre_plan_approval_mode
        .unwrap_or_else(ApprovalMode::default);
    state.plan_approval_dialog = None;
    state.enter_running();

    let _ = action_tx.send(UserAction::ImplementApprovedPlan {
        prompt: IMPLEMENT_APPROVED_PLAN_PROMPT.to_string(),
        approval_mode: target_mode,
    });
    state.request_runtime_queue_start();
    state.resume_queued_follow_up_autosend();
}

fn stay_in_plan_mode(state: &mut AppState) {
    state.plan_approval_dialog = None;
    state.request_runtime_queue_start();
    state.resume_queued_follow_up_autosend();
    state.push_message(crate::transcript_state::ChatMessage::System(
        "Staying in Plan mode. Send feedback to revise the plan.".to_string(),
    ));
    state.scroll_to_bottom();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PlanApprovalDialog;

    fn state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.approval_mode = ApprovalMode::Plan;
        state.pre_plan_approval_mode = Some(ApprovalMode::FullAuto);
        state.plan_approval_dialog = Some(PlanApprovalDialog {
            plan: "- inspect\n- implement".to_string(),
            selected: 0,
        });
        state.suspend_queued_follow_up_autosend();
        state
    }

    #[test]
    fn approving_restores_previous_mode_before_submitting_implementation() {
        let (tx, rx) = mpsc::unbounded();
        let mut state = state();

        handle_plan_approval_key(
            &KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &mut state,
            &tx,
        );

        assert!(state.plan_approval_dialog.is_none());
        assert_eq!(state.status, crate::types::AppStatus::Running);
        assert!(state.queued_autosend_enabled());
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ImplementApprovedPlan {
                prompt,
                approval_mode: ApprovalMode::FullAuto,
            }) if prompt == IMPLEMENT_APPROVED_PLAN_PROMPT
        ));
    }

    #[test]
    fn rejecting_keeps_plan_mode_and_does_not_submit() {
        let (tx, rx) = mpsc::unbounded();
        let mut state = state();
        state.plan_approval_dialog.as_mut().unwrap().selected = 1;

        handle_plan_approval_key(
            &KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &mut state,
            &tx,
        );

        assert!(state.plan_approval_dialog.is_none());
        assert_eq!(state.approval_mode, ApprovalMode::Plan);
        assert_eq!(state.status, crate::types::AppStatus::Idle);
        assert!(rx.try_recv().is_err());
    }
}
