use crossbeam_channel as mpsc;
use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest};
use orca_core::config::RunConfig;

use crate::protocol::UserAction;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus};

/// A turn moved to the background while its model request ran can run no
/// tool on its own: each tool call its model returns parks the turn, and
/// answering that approval is what resumes it. The ones the session's
/// policy would have run in the foreground without asking (the approval
/// mode, the configured permission rules, or a tool allowed for this
/// session) resume by themselves once no turn runs here; the rest wait for
/// the user, as before.
pub(crate) fn continue_allowed_background_approvals(
    state: &mut AppState,
    config: &RunConfig,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if state.status != AppStatus::Idle {
        return;
    }
    let policy = ApprovalPolicy::new(state.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let allowed = state
        .workflow_tasks()
        .iter()
        .filter(|task| crate::workflow_panel::is_pending_background_approval(task))
        .filter_map(|task| task.pending_tool_call.as_ref())
        .filter(|call| !state.continued_background_approvals.contains(&call.id))
        .filter(|call| {
            let request = ApprovalRequest {
                id: call.id.clone(),
                action: call.action,
                description: String::new(),
                tool: Some(call.name.clone()),
                target: call.target.clone(),
                preview: None,
            };
            policy
                .resolve_for_tool(&request, &call.name, call.target.as_deref())
                .decision
                == ApprovalDecision::Allow
                || state.approval_is_allowlisted(&call.name, call.target.as_deref())
        })
        .map(|call| (call.id.clone(), call.name.clone(), call.target.clone()))
        .collect::<Vec<_>>();
    for (id, tool, target) in allowed {
        state.continued_background_approvals.insert(id.clone());
        let target = target
            .map(|target| format!(" {target}"))
            .unwrap_or_default();
        state.push_message(ChatMessage::System {
            text: format!(
                "Background session continues with {tool}{target}: {} runs it without asking.",
                state.approval_mode.as_str()
            ),
            expanded: false,
        });
        let _ = action_tx.send(UserAction::ResolveBackgroundApproval { id, approved: true });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::task_types::{
        BackgroundTaskSummary, PendingToolCallSummary, TaskLifetime, TaskStatus, TaskType,
    };

    use crate::protocol::TuiEvent;
    use crate::types::AppStatus;

    fn parked(action: ActionKind, tool: &str) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: "deploy".to_string(),
            parent_task_id: None,
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            lifetime: TaskLifetime::Task,
            description: "deploy".to_string(),
            created_at_ms: 1,
            started_at_ms: Some(1),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some(tool.to_string()),
            pending_tool_call: Some(PendingToolCallSummary {
                id: "deploy-call".to_string(),
                name: tool.to_string(),
                action,
                target: Some("README.md".to_string()),
                arguments: "{}".to_string(),
            }),
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: None,
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: None,
            last_activity_at_ms: Some(2),
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: Some(3),
        }
    }

    fn resumed(
        mode: ApprovalMode,
        task: BackgroundTaskSummary,
        status: AppStatus,
        config: &RunConfig,
    ) -> Vec<(String, bool)> {
        let (tx, rx) = mpsc::unbounded();
        let mut state = AppState::new(tx.clone(), "test".into(), "mock".into(), "/tmp".into());
        state.approval_mode = mode;
        state.status = status;
        state.replace_workflow_tasks_for_test(vec![task]);
        continue_allowed_background_approvals(&mut state, config, &tx);
        // Asking twice never resumes a turn twice.
        continue_allowed_background_approvals(&mut state, config, &tx);
        rx.try_iter()
            .filter_map(|action| match action {
                UserAction::ResolveBackgroundApproval { id, approved } => Some((id, approved)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_parked_tool_call_the_mode_runs_unasked_continues_by_itself() {
        let config = crate::test_support::test_run_config();
        let once = vec![("deploy-call".to_string(), true)];
        assert_eq!(
            resumed(
                ApprovalMode::AutoEdit,
                parked(ActionKind::Write, "edit"),
                AppStatus::Idle,
                &config
            ),
            once
        );
        assert_eq!(
            resumed(
                ApprovalMode::FullAuto,
                parked(ActionKind::Shell, "bash"),
                AppStatus::Idle,
                &config
            ),
            once
        );
        // A read never asks, in any mode that can run tools.
        assert_eq!(
            resumed(
                ApprovalMode::Suggest,
                parked(ActionKind::Read, "read"),
                AppStatus::Idle,
                &config
            ),
            once
        );
    }

    #[test]
    fn a_parked_tool_call_the_mode_asks_about_still_waits_for_the_user() {
        let config = crate::test_support::test_run_config();
        assert!(
            resumed(
                ApprovalMode::Suggest,
                parked(ActionKind::Write, "edit"),
                AppStatus::Idle,
                &config
            )
            .is_empty()
        );
        assert!(
            resumed(
                ApprovalMode::Plan,
                parked(ActionKind::Write, "edit"),
                AppStatus::Idle,
                &config
            )
            .is_empty()
        );
    }

    #[test]
    fn a_parked_tool_call_waits_while_a_turn_runs_here() {
        let config = crate::test_support::test_run_config();
        assert!(
            resumed(
                ApprovalMode::AutoEdit,
                parked(ActionKind::Write, "edit"),
                AppStatus::Running,
                &config
            )
            .is_empty()
        );
    }

    fn system_lines(state: &AppState) -> Vec<String> {
        state
            .transcript
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::System { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn parked_state(mode: ApprovalMode) -> (AppState, mpsc::Sender<UserAction>) {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(tx.clone(), "test".into(), "mock".into(), "/tmp".into());
        state.approval_mode = mode;
        state.status = AppStatus::Idle;
        state.replace_workflow_tasks_for_test(vec![parked(ActionKind::Write, "edit")]);
        (state, tx)
    }

    fn approval_needed() -> TuiEvent {
        TuiEvent::BackgroundApprovalNeeded {
            call_id: Some("deploy-call".to_string()),
            tool: Some("edit".to_string()),
        }
    }

    #[test]
    fn a_continued_tool_call_is_not_announced_as_waiting_afterwards() {
        let config = crate::test_support::test_run_config();
        let continued =
            "Background session continues with edit README.md: auto-edit runs it without asking.";
        let waiting = "Background session needs approval for edit before it can continue.";

        // Continued before the background session reports the approval.
        let (mut state, tx) = parked_state(ApprovalMode::AutoEdit);
        continue_allowed_background_approvals(&mut state, &config, &tx);
        state.update(approval_needed());
        assert_eq!(system_lines(&state), [continued]);

        // Reported first: the continuation follows it.
        let (mut state, tx) = parked_state(ApprovalMode::AutoEdit);
        state.update(approval_needed());
        continue_allowed_background_approvals(&mut state, &config, &tx);
        assert_eq!(system_lines(&state), [waiting, continued]);

        // One the user answers is announced.
        let (mut state, tx) = parked_state(ApprovalMode::Suggest);
        continue_allowed_background_approvals(&mut state, &config, &tx);
        state.update(approval_needed());
        assert_eq!(system_lines(&state), [waiting]);
    }

    #[test]
    fn a_permission_rule_that_prompts_keeps_the_tool_call_waiting() {
        let mut config = crate::test_support::test_run_config();
        config.permission_rules = orca_core::approval_rules::PermissionRules {
            rules: vec![orca_core::approval_rules::PermissionRule::new(
                "edit",
                "*",
                orca_core::approval_types::Decision::Prompt,
            )],
        };
        assert!(
            resumed(
                ApprovalMode::AutoEdit,
                parked(ActionKind::Write, "edit"),
                AppStatus::Idle,
                &config
            )
            .is_empty()
        );
    }
}
