use crossbeam_channel as mpsc;

use orca_core::approval_rules::CompiledPermissionRules;
use orca_core::approval_types::Decision;
use orca_core::mcp_types::mcp_tool_server;

use crate::protocol::{
    TuiInteractionKind, TuiInteractionResponse, TuiPermissionDecision, UserAction,
};
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus, ApprovalOption};

/// Resolve the approval dialog by the chosen option. The "always allow"
/// options record a session allowlist entry so later matching approvals are
/// auto-granted by the app event loop; the two "(saved)" options (MCP tools
/// only) additionally persist an allow rule to the user config so future
/// sessions skip the prompt too — a write failure is reported through the
/// transcript, but does not undo the session's grant. Tool approvals stay a
/// simple allow/deny bool; permission requests carry the typed scope so
/// "always" reaches the runtime as a session grant instead of a single turn.
pub(crate) fn resolve_approval_option(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    option: ApprovalOption,
) {
    // Pulled out as owned values up front: the saved-allow branches need
    // `&mut state` as a whole (to report a save failure), which cannot
    // coexist with a borrow of `state.approval_dialog`.
    let dialog_tool = state
        .approval_dialog
        .as_ref()
        .map(|dialog| dialog.tool.clone());
    let dialog_target = state
        .approval_dialog
        .as_ref()
        .and_then(|dialog| dialog.target.clone());

    match option {
        ApprovalOption::AlwaysTool => {
            if let Some(tool) = &dialog_tool {
                state
                    .approval_allowlist
                    .insert(AppState::approval_key_tool(tool));
            }
        }
        ApprovalOption::AlwaysTarget => {
            if let (Some(tool), Some(target)) = (&dialog_tool, &dialog_target) {
                state
                    .approval_allowlist
                    .insert(AppState::approval_key_target(tool, target));
            }
        }
        ApprovalOption::AlwaysToolSaved => {
            if let Some(tool) = &dialog_tool {
                state
                    .approval_allowlist
                    .insert(AppState::approval_key_tool(tool));
                save_allow_rule(state, tool, tool, dialog_target.as_deref());
            }
        }
        ApprovalOption::AlwaysServerSaved => {
            let rule_tool = dialog_tool
                .as_deref()
                .and_then(mcp_tool_server)
                .map(AppState::approval_key_mcp_server);
            if let (Some(rule_tool), Some(tool)) = (rule_tool, &dialog_tool) {
                state.approval_allowlist.insert(rule_tool.clone());
                save_allow_rule(state, &rule_tool, tool, dialog_target.as_deref());
            }
        }
        ApprovalOption::Once | ApprovalOption::Deny => {}
    }
    resolve_approval(state, action_tx, option);
}

/// Persist a saved-allow rule for `rule_tool` (a bare tool name for
/// `AlwaysToolSaved`, or `mcp__<server>__*` for `AlwaysServerSaved`) to the
/// user config, for the call of `tool` on `target` being approved. The
/// session grant above already stands either way, so a write failure is
/// reported to the transcript instead of undoing it. So is a rule of the
/// user config that still decides that call ahead of the saved one.
fn save_allow_rule(state: &mut AppState, rule_tool: &str, tool: &str, target: Option<&str>) {
    let text = match orca_core::config::user_edit::add_user_allow_rule(rule_tool) {
        Err(error) => format!(
            "Allowed for this session, but saving the rule to the user config failed: {error}"
        ),
        Ok(_) if stricter_user_rule_applies(tool, target) => {
            format!("saved, but a stricter rule in your config still applies to {tool}")
        }
        Ok(_) => return,
    };
    state.push_message(ChatMessage::System {
        text,
        expanded: false,
    });
}

/// Whether a `deny` or `prompt` rule of the user config matches a call of
/// `tool` on `target`, as the approval policy matches rules (MCP names in
/// their canonical form, server rules, patterns): the strictest matching
/// rule decides a call, so an allow rule saved beside it does not. A config
/// that cannot be read is taken to have none.
fn stricter_user_rule_applies(tool: &str, target: Option<&str>) -> bool {
    orca_core::config::user_edit::user_permission_rules().is_ok_and(|rules| {
        matches!(
            CompiledPermissionRules::from_rules(rules).matching_decision(tool, target),
            Some(Decision::Deny | Decision::Prompt)
        )
    })
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

/// Map a chosen approval option to the typed permission decision. Every
/// persistent "always" option — session-only or saved to the user config —
/// is the user's request to persist the grant for the rest of the session;
/// "allow this once" stays turn-scoped.
fn permission_decision_for(option: ApprovalOption) -> TuiPermissionDecision {
    match option {
        ApprovalOption::Once => TuiPermissionDecision::AllowOnce,
        ApprovalOption::AlwaysTool
        | ApprovalOption::AlwaysTarget
        | ApprovalOption::AlwaysToolSaved
        | ApprovalOption::AlwaysServerSaved => TuiPermissionDecision::AllowSession,
        ApprovalOption::Deny => TuiPermissionDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_options_map_to_a_session_scoped_grant() {
        // The previous bool wire form collapsed "always" to a single turn.
        // Every persistent option must now reach the runtime as a session grant.
        for option in [
            ApprovalOption::AlwaysTool,
            ApprovalOption::AlwaysTarget,
            ApprovalOption::AlwaysToolSaved,
            ApprovalOption::AlwaysServerSaved,
        ] {
            assert_eq!(
                permission_decision_for(option),
                TuiPermissionDecision::AllowSession,
                "{option:?}"
            );
        }
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

    /// Like `state_awaiting_a_tool_approval`, but for an MCP tool call: no
    /// target (MCP tool calls have none), and `options_for` therefore offers
    /// the two saved-allow options instead of `AlwaysTarget`.
    fn state_awaiting_an_mcp_tool_approval(tool: &str) -> AppState {
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
            tool: tool.to_string(),
            target: None,
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: crate::types::ApprovalDialog::options_for(tool, None),
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
        assert!(permission_decision_for(ApprovalOption::AlwaysToolSaved).is_allow());
        assert!(permission_decision_for(ApprovalOption::AlwaysServerSaved).is_allow());
        assert!(!permission_decision_for(ApprovalOption::Deny).is_allow());
    }

    #[test]
    fn saving_a_server_allow_covers_the_servers_other_tools_this_session() {
        let home = crate::test_support::isolate_orca_home();
        let mut state = state_awaiting_an_mcp_tool_approval("mcp__github__create_issue");
        let (action_tx, _action_rx) = mpsc::unbounded();

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::AlwaysServerSaved);

        // Another tool on the same server is covered this session...
        assert!(state.approval_is_allowlisted("mcp__github__list_issues", None));
        // ...but a different server is not.
        assert!(!state.approval_is_allowlisted("mcp__gitlab__list_issues", None));

        let config =
            std::fs::read_to_string(home.path().join("config.toml")).expect("config written");
        assert!(
            config.contains(r#"tool = "mcp__github__*""#),
            "server rule missing: {config}"
        );
    }

    #[test]
    fn saving_an_allow_rule_under_a_stricter_rule_warns() {
        let stricter = |tool: &str| {
            format!("saved, but a stricter rule in your config still applies to {tool}")
        };
        let cases = [
            // A prompt rule for the tool, written with Orca's other spelling.
            (
                "[[permissions.rules]]\ntool = \"mcp__GitHub__Create-Issue\"\ndecision = \"prompt\"\n",
                ApprovalOption::AlwaysToolSaved,
                Some(stricter("mcp__github__create_issue")),
            ),
            // A deny rule for its whole server, in an inline array.
            (
                "permissions.rules = [{ tool = \"mcp__github\", decision = \"deny\" }]\n",
                ApprovalOption::AlwaysServerSaved,
                Some(stricter("mcp__github__create_issue")),
            ),
            // Rules that do not match the call, as the policy matches them:
            // another server, and a pattern its target does not match.
            (
                "[[permissions.rules]]\ntool = \"mcp__gitlab__*\"\ndecision = \"deny\"\n\n\
                 [[permissions.rules]]\ntool = \"mcp__github__create_issue\"\npattern = \"src/**\"\ndecision = \"deny\"\n",
                ApprovalOption::AlwaysToolSaved,
                None,
            ),
            // An allow rule is no stricter.
            (
                "[[permissions.rules]]\ntool = \"mcp__github__*\"\ndecision = \"allow\"\n",
                ApprovalOption::AlwaysToolSaved,
                None,
            ),
        ];
        for (config, option, notice) in cases {
            let home = crate::test_support::isolate_orca_home();
            std::fs::write(home.path().join("config.toml"), config).expect("seed config");
            let mut state = state_awaiting_an_mcp_tool_approval("mcp__github__create_issue");
            let (action_tx, action_rx) = mpsc::unbounded();

            resolve_approval_option(&mut state, &action_tx, option);

            // The call runs, and the rest of the session skips the prompt.
            assert_eq!(state.status, AppStatus::Running, "{config}");
            assert!(
                state.approval_is_allowlisted("mcp__github__create_issue", None),
                "{config}"
            );
            assert!(action_rx.try_recv().is_ok(), "{config}");
            let written = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
            assert!(written.contains("decision = \"allow\""), "{written}");
            let said = state
                .transcript
                .messages
                .iter()
                .filter_map(|message| match message {
                    crate::transcript_state::ChatMessage::System { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(said, Vec::from_iter(notice), "{config}");
        }
    }

    #[test]
    fn a_failed_save_still_allows_and_says_why() {
        let home = crate::test_support::isolate_orca_home();
        std::fs::write(home.path().join("config.toml"), "model = [\n").expect("seed broken config");
        let mut state = state_awaiting_an_mcp_tool_approval("mcp__github__create_issue");
        let (action_tx, action_rx) = mpsc::unbounded();

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::AlwaysToolSaved);

        // The call is still allowed this session even though the save failed.
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.approval_is_allowlisted("mcp__github__create_issue", None));
        assert!(action_rx.try_recv().is_ok(), "an approval response is sent");
        let last = state.transcript.messages.last().expect("a system message");
        assert!(
            matches!(last, crate::transcript_state::ChatMessage::System { text, .. }
                if text.contains("saving the rule to the user config failed")),
            "got {last:?}"
        );
    }
}
