use orca_core::config::RunConfig;

use crate::protocol::UserAction;
use crate::slash_command_actions::encode_settings_intent;
use crate::types::AppState;

pub(crate) fn cycle_approval_mode(
    config: &RunConfig,
    state: &mut AppState,
    action_tx: &crossbeam_channel::Sender<UserAction>,
) {
    let next = config.approval_mode.next();
    let _ = action_tx.send(UserAction::SetModel(encode_settings_intent(
        None,
        None,
        Some(next),
    )));
    state.push_message(crate::transcript_state::ChatMessage::System(format!(
        "Approval mode change requested: {}.",
        next.as_str()
    )));
    state.scroll_to_bottom();
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

        cycle_approval_mode(&config, &mut state, &action_tx);

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
}
