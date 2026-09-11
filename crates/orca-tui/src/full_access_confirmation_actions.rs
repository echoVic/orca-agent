use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use orca_core::approval_types::ApprovalMode;
use orca_core::config::ReasoningEffort;

use crate::protocol::UserAction;
use crate::slash_command_actions::{encode_confirmed_full_access_intent, encode_settings_intent};
use crate::types::{AppState, FullAccessConfirmation};

/// Routes every TUI settings change through the Full Access confirmation
/// boundary. Returning `false` means the intent is pending user confirmation.
pub(crate) fn request_settings_change(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    approval_mode: Option<ApprovalMode>,
) -> bool {
    if approval_mode == Some(ApprovalMode::FullAuto)
        && state.approval_mode != ApprovalMode::FullAuto
    {
        state.full_access_confirmation = Some(FullAccessConfirmation {
            selected: 1,
            model,
            reasoning_effort,
        });
        return false;
    }

    let _ = action_tx.send(UserAction::SetModel(encode_settings_intent(
        model.as_deref(),
        reasoning_effort,
        approval_mode,
    )));
    true
}

pub(crate) fn handle_full_access_confirmation_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }

    match key.code {
        KeyCode::Esc => {
            state.full_access_confirmation = None;
        }
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Tab
        | KeyCode::BackTab => {
            if let Some(confirmation) = state.full_access_confirmation.as_mut() {
                confirmation.selected = usize::from(confirmation.selected == 0);
            }
        }
        KeyCode::Enter => {
            let Some(confirmation) = state.full_access_confirmation.take() else {
                return;
            };
            if confirmation.selected == 0 {
                let _ = action_tx.send(UserAction::SetModel(encode_confirmed_full_access_intent(
                    confirmation.model.as_deref(),
                    confirmation.reasoning_effort,
                )));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_full_access_confirmation_key, request_settings_change};
    use crate::protocol::UserAction;
    use crate::slash_command_actions::decode_settings_intent;
    use crate::types::AppState;
    use crossbeam_channel as mpsc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use orca_core::approval_types::ApprovalMode;

    fn state(action_tx: mpsc::Sender<UserAction>) -> AppState {
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        );
        state.approval_mode = ApprovalMode::AutoEdit;
        state
    }

    #[test]
    fn full_access_defaults_to_cancel_and_dispatches_only_after_confirmation() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state(action_tx.clone());

        assert!(!request_settings_change(
            &mut state,
            &action_tx,
            None,
            None,
            Some(ApprovalMode::FullAuto),
        ));
        assert_eq!(
            state
                .full_access_confirmation
                .as_ref()
                .map(|confirmation| confirmation.selected),
            Some(1)
        );
        assert!(action_rx.try_recv().is_err());

        handle_full_access_confirmation_key(
            &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut state,
            &action_tx,
        );
        handle_full_access_confirmation_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &action_tx,
        );

        let UserAction::SetModel(encoded) = action_rx.try_recv().expect("settings action") else {
            panic!("expected settings action");
        };
        let intent = decode_settings_intent(&encoded).expect("settings intent");
        assert_eq!(intent.approval_mode, Some(ApprovalMode::FullAuto));
        assert!(intent.full_access_confirmed);
        assert!(state.full_access_confirmation.is_none());
    }

    #[test]
    fn escape_cancels_without_runtime_mutation() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state(action_tx.clone());
        request_settings_change(
            &mut state,
            &action_tx,
            Some("deepseek-flash".to_string()),
            None,
            Some(ApprovalMode::FullAuto),
        );

        handle_full_access_confirmation_key(
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &action_tx,
        );

        assert!(state.full_access_confirmation.is_none());
        assert!(action_rx.try_recv().is_err());
    }
}
