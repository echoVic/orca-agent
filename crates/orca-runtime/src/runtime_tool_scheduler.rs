use orca_core::config::RunConfig;
use orca_core::tool_types::ToolRequest;

use crate::runtime_readonly_tool_turn::{collect_readonly_batch, should_run_readonly_batch};
use crate::step_context::{RuntimeSamplingRequestState, RuntimeToolDispatchWindow};
use crate::subagent_execution::{collect_subagent_batch, should_run_subagent_batch};

pub(crate) enum RuntimeToolDispatch<'a> {
    Normal(&'a ToolRequest),
    ReadonlyBatch(RuntimeToolDispatchWindow<'a>),
    SubagentBatch(RuntimeToolDispatchWindow<'a>),
}

pub(crate) struct RuntimeToolDispatchScheduler<'a> {
    config: &'a RunConfig,
    subagent_depth: u32,
}

impl<'a> RuntimeToolDispatchScheduler<'a> {
    pub(crate) fn new(config: &'a RunConfig, subagent_depth: u32) -> Self {
        Self {
            config,
            subagent_depth,
        }
    }

    pub(crate) fn next_dispatch<'requests>(
        &self,
        sampling_state: &RuntimeSamplingRequestState,
        tool_requests: &'requests [ToolRequest],
    ) -> Option<RuntimeToolDispatch<'requests>> {
        let current = sampling_state.current_tool_request(tool_requests)?;

        if should_run_subagent_batch(self.config, current, self.subagent_depth) {
            let window = sampling_state.tool_dispatch_window(tool_requests, |requests, start| {
                collect_subagent_batch(self.config, requests, start)
            });
            return Some(RuntimeToolDispatch::SubagentBatch(window));
        }

        if should_run_readonly_batch(self.config.tools.max_read_parallel, current) {
            let window = sampling_state.tool_dispatch_window(tool_requests, |requests, start| {
                collect_readonly_batch(self.config.tools.max_read_parallel, requests, start)
            });
            return Some(RuntimeToolDispatch::ReadonlyBatch(window));
        }

        Some(RuntimeToolDispatch::Normal(current))
    }
}

#[cfg(test)]
mod tests {
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use serde_json::json;

    use super::*;
    use crate::step_context::RuntimeSamplingRequestState;

    fn config() -> RunConfig {
        RunConfig {
            prompt: "test".to_string(),
            app_version: "test".to_string(),
            cwd: None,
            provider: ProviderKind::Mock,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            approval_mode: ApprovalMode::Suggest,
            output_format: OutputFormat::Jsonl,
            verifier: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            theme: ThemeName::Dark,
            mcp_servers: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            hooks: Vec::new(),
            workflows: WorkflowConfig::default(),
            subagents: SubagentConfig {
                max_depth: 1,
                max_parallel: 3,
                ..SubagentConfig::default()
            },
            tools: ToolConfig {
                max_read_parallel: 3,
                ..ToolConfig::default()
            },
            external_tools: Vec::new(),
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn request(id: &str, name: ToolName, action: ActionKind) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name,
            action,
            target: Some(id.to_string()),
            raw_arguments: None,
        }
    }

    fn subagent_request(id: &str) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some(format!("inspect {id}")),
            raw_arguments: Some(
                json!({
                    "description": format!("inspect {id}"),
                    "prompt": format!("inspect {id}")
                })
                .to_string(),
            ),
        }
    }

    fn ids<'a>(dispatch: &'a RuntimeToolDispatch<'a>) -> Vec<&'a str> {
        match dispatch {
            RuntimeToolDispatch::Normal(request) => vec![request.id.as_str()],
            RuntimeToolDispatch::ReadonlyBatch(window)
            | RuntimeToolDispatch::SubagentBatch(window) => window
                .tool_requests()
                .iter()
                .map(|request| request.id.as_str())
                .collect(),
        }
    }

    #[test]
    fn scheduler_batches_adjacent_readonly_requests_until_first_non_readonly_boundary() {
        let config = config();
        let requests = vec![
            request("read-a", ToolName::ReadFile, ActionKind::Read),
            request("read-b", ToolName::ListFiles, ActionKind::Read),
            request("write-a", ToolName::WriteFile, ActionKind::Write),
            request("read-c", ToolName::ReadFile, ActionKind::Read),
        ];
        let sampling_state = RuntimeSamplingRequestState::new();

        let dispatch = RuntimeToolDispatchScheduler::new(&config, 0)
            .next_dispatch(&sampling_state, &requests)
            .expect("dispatch");

        assert!(matches!(dispatch, RuntimeToolDispatch::ReadonlyBatch(_)));
        assert_eq!(ids(&dispatch), vec!["read-a", "read-b"]);
    }

    #[test]
    fn scheduler_keeps_goal_control_tools_out_of_readonly_batches() {
        let config = config();
        let requests = vec![
            request("read-a", ToolName::ReadFile, ActionKind::Read),
            request("goal-update", ToolName::UpdateGoal, ActionKind::Read),
            request("read-b", ToolName::ReadFile, ActionKind::Read),
        ];
        let mut sampling_state = RuntimeSamplingRequestState::new();

        let first = RuntimeToolDispatchScheduler::new(&config, 0)
            .next_dispatch(&sampling_state, &requests)
            .expect("first dispatch");
        assert!(matches!(first, RuntimeToolDispatch::ReadonlyBatch(_)));
        assert_eq!(ids(&first), vec!["read-a"]);

        sampling_state.advance_tool_cursor_one(requests.len());
        let second = RuntimeToolDispatchScheduler::new(&config, 0)
            .next_dispatch(&sampling_state, &requests)
            .expect("goal dispatch");
        assert!(matches!(second, RuntimeToolDispatch::Normal(_)));
        assert_eq!(ids(&second), vec!["goal-update"]);
    }

    #[test]
    fn scheduler_batches_adjacent_sync_subagents_until_first_non_batchable_boundary() {
        let config = config();
        let async_request = ToolRequest {
            id: "agent-async".to_string(),
            raw_arguments: Some(
                json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
            ..subagent_request("agent-async")
        };
        let requests = vec![
            subagent_request("agent-a"),
            subagent_request("agent-b"),
            async_request,
            subagent_request("agent-c"),
        ];
        let sampling_state = RuntimeSamplingRequestState::new();

        let dispatch = RuntimeToolDispatchScheduler::new(&config, 0)
            .next_dispatch(&sampling_state, &requests)
            .expect("dispatch");

        assert!(matches!(dispatch, RuntimeToolDispatch::SubagentBatch(_)));
        assert_eq!(ids(&dispatch), vec!["agent-a", "agent-b"]);
    }

    #[test]
    fn scheduler_dispatches_non_batchable_current_request_as_normal_single_tool() {
        let config = config();
        let requests = vec![
            request("shell-a", ToolName::Bash, ActionKind::Shell),
            request("read-a", ToolName::ReadFile, ActionKind::Read),
        ];
        let sampling_state = RuntimeSamplingRequestState::new();

        let dispatch = RuntimeToolDispatchScheduler::new(&config, 0)
            .next_dispatch(&sampling_state, &requests)
            .expect("dispatch");

        assert!(matches!(dispatch, RuntimeToolDispatch::Normal(_)));
        assert_eq!(ids(&dispatch), vec!["shell-a"]);
    }

    #[test]
    fn scheduler_returns_none_when_sampling_state_has_no_current_request() {
        let config = config();
        let requests = vec![request("read-a", ToolName::ReadFile, ActionKind::Read)];
        let mut sampling_state = RuntimeSamplingRequestState::new();
        sampling_state.advance_tool_cursor_one(requests.len());

        assert!(
            RuntimeToolDispatchScheduler::new(&config, 0)
                .next_dispatch(&sampling_state, &requests)
                .is_none()
        );
    }
}
