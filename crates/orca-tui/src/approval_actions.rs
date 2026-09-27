use crossbeam_channel as mpsc;

use crate::protocol::{
    TuiInteractionKind, TuiInteractionResponse, TuiPermissionDecision, UserAction,
};
use crate::types::{AppState, AppStatus, ApprovalOption};

/// Resolve the approval dialog by the chosen option. The "always allow"
/// options record a session allowlist entry so later matching approvals are
/// auto-granted by the app event loop. Tool approvals stay a simple allow/deny
/// bool; permission requests carry the typed scope so "always" reaches the
/// runtime as a session grant instead of a single turn.
pub(crate) fn resolve_approval_option(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    option: ApprovalOption,
) {
    if let Some(dialog) = &state.approval_dialog {
        match option {
            ApprovalOption::AlwaysTool => {
                state
                    .approval_allowlist
                    .insert(AppState::approval_key_tool(&dialog.tool));
            }
            ApprovalOption::AlwaysTarget => {
                if let Some(target) = &dialog.target {
                    state
                        .approval_allowlist
                        .insert(AppState::approval_key_target(&dialog.tool, target));
                }
            }
            ApprovalOption::Once | ApprovalOption::Deny => {}
        }
    }
    resolve_approval(state, action_tx, option);
}

fn resolve_approval(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    option: ApprovalOption,
) {
    let approved = option.is_approve();
    if state
        .approval_dialog
        .as_ref()
        .and_then(|dialog| dialog.background_task_id.as_ref())
        .is_some()
    {
        let Some(id) = state
            .approval_dialog
            .as_ref()
            .map(|dialog| dialog.id.clone())
        else {
            return;
        };
        let _ = action_tx.send(UserAction::ResolveBackgroundApproval { id, approved });
        state.set_status(AppStatus::Idle);
    } else {
        let Some(interaction) = state
            .approval_dialog
            .as_ref()
            .and_then(|dialog| dialog.interaction.clone())
        else {
            return;
        };
        let response = match interaction.kind {
            TuiInteractionKind::Approval => TuiInteractionResponse::Approval(approved),
            TuiInteractionKind::Permission => {
                TuiInteractionResponse::Permission(permission_decision_for(option))
            }
            TuiInteractionKind::UserInput | TuiInteractionKind::McpElicitation => return,
        };
        let _ = action_tx.send(UserAction::RespondToInteraction {
            key: interaction,
            response,
        });
        if approved {
            state.enter_running();
        } else {
            state.denied_approval_stops_turn = true;
            state.set_status(AppStatus::Idle);
        }
    }
    state.approval_dialog = None;
}

/// Map a chosen approval option to the typed permission decision. The two
/// "always" options are the user's request to persist the grant for the rest
/// of the session; "allow this once" stays turn-scoped.
fn permission_decision_for(option: ApprovalOption) -> TuiPermissionDecision {
    match option {
        ApprovalOption::Once => TuiPermissionDecision::AllowOnce,
        ApprovalOption::AlwaysTool | ApprovalOption::AlwaysTarget => {
            TuiPermissionDecision::AllowSession
        }
        ApprovalOption::Deny => TuiPermissionDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_options_map_to_a_session_scoped_grant() {
        // The previous bool wire form collapsed "always" to a single turn.
        // Both persistent options must now reach the runtime as a session grant.
        assert_eq!(
            permission_decision_for(ApprovalOption::AlwaysTool),
            TuiPermissionDecision::AllowSession
        );
        assert_eq!(
            permission_decision_for(ApprovalOption::AlwaysTarget),
            TuiPermissionDecision::AllowSession
        );
    }

    #[test]
    fn once_stays_turn_scoped_and_deny_is_a_rejection() {
        assert_eq!(
            permission_decision_for(ApprovalOption::Once),
            TuiPermissionDecision::AllowOnce
        );
        assert_eq!(
            permission_decision_for(ApprovalOption::Deny),
            TuiPermissionDecision::Deny
        );
    }

    fn state_awaiting_a_tool_approval() -> AppState {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            event_tx,
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::WaitingApproval;
        state.approval_dialog = Some(crate::types::ApprovalDialog {
            id: "approval-1".to_string(),
            interaction: Some(crate::protocol::TuiInteractionKey::new(
                orca_core::cancel::OperationIdAllocator::new().allocate(),
                "approval-1",
                TuiInteractionKind::Approval,
            )),
            tool: "edit".to_string(),
            target: Some("billing/pages.py".to_string()),
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: crate::types::ApprovalDialog::options_for("edit", Some("billing/pages.py")),
            diff: None,
            diff_scroll: 0,
        });
        state
    }

    fn approval_unavailable_card() -> crate::protocol::TuiEvent {
        crate::protocol::TuiEvent::Diagnostic(
            crate::diagnostics::TuiDiagnostic::from_surface_terminal(
                &orca_runtime::surface::OperationTerminal::Failed {
                    class: orca_runtime::surface::FailureClass::LegacyApprovalRequired,
                    message: orca_runtime::surface::SafeDiagnosticText::try_new("denied in TUI")
                        .unwrap(),
                },
            )
            .expect("a failed terminal has a diagnostic"),
        )
    }

    #[test]
    fn a_turn_stopped_by_your_denial_ends_with_a_note_not_an_error_card() {
        let mut state = state_awaiting_a_tool_approval();
        let (action_tx, _action_rx) = mpsc::unbounded();

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::Deny);
        state.update(approval_unavailable_card());

        let last = state.transcript.messages.last().expect("a closing message");
        assert!(
            matches!(last, crate::transcript_state::ChatMessage::System { text, .. }
                if text.starts_with("Denied.")),
            "got {last:?}"
        );
    }

    #[test]
    fn an_approval_nobody_could_answer_still_shows_the_error_card() {
        let mut state = state_awaiting_a_tool_approval();

        state.update(approval_unavailable_card());

        assert!(matches!(
            state.transcript.messages.last(),
            Some(crate::transcript_state::ChatMessage::Diagnostic(_))
        ));
    }

    #[test]
    fn only_deny_is_not_an_allow() {
        assert!(permission_decision_for(ApprovalOption::Once).is_allow());
        assert!(permission_decision_for(ApprovalOption::AlwaysTool).is_allow());
        assert!(permission_decision_for(ApprovalOption::AlwaysTarget).is_allow());
        assert!(!permission_decision_for(ApprovalOption::Deny).is_allow());
    }
}
