use crate::full_access_confirmation_actions::request_settings_change;
use crate::protocol::UserAction;
use crate::types::AppState;

pub(crate) fn cycle_approval_mode(
    state: &mut AppState,
    action_tx: &crossbeam_channel::Sender<UserAction>,
) {
    let next = state.approval_mode.next();
    let dispatched = request_settings_change(state, action_tx, None, None, Some(next));
    if dispatched {
        state.push_message(crate::transcript_state::ChatMessage::System(format!(
            "Approval mode change requested: {}.",
            next.as_str()
        )));
        state.scroll_to_bottom();
    }
}

#[cfg(test)]
mod tests {
    use super::cycle_approval_mode;
    use crate::protocol::UserAction;
    use crate::slash_command_actions::decode_settings_intent;
    use crate::types::AppState;
    use orca_core::approval_types::ApprovalMode;

    #[test]
    fn approval_mode_cycle_submits_runtime_settings_without_local_mutation() {
        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        let mut config = crate::test_support::test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.approval_mode = ApprovalMode::Suggest;

        cycle_approval_mode(&mut state, &action_tx);

        assert_eq!(config.approval_mode, ApprovalMode::Suggest);
        assert_eq!(state.approval_mode, ApprovalMode::Suggest);
        let action = action_rx.try_recv().expect("settings action");
        let UserAction::SetModel(encoded) = action else {
            panic!("expected typed settings action");
        };
        assert_eq!(
            decode_settings_intent(&encoded)
                .expect("settings intent")
                .approval_mode,
            Some(ApprovalMode::AutoEdit)
        );
    }

    #[test]
    fn approval_mode_cycle_requires_confirmation_before_full_access() {
        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.approval_mode = ApprovalMode::AutoEdit;

        cycle_approval_mode(&mut state, &action_tx);

        assert_eq!(state.approval_mode, ApprovalMode::AutoEdit);
        assert!(state.full_access_confirmation.is_some());
        assert!(action_rx.try_recv().is_err());
    }
}
