use orca_approval::ApprovalPolicy;
use orca_core::approval_types::ApprovalDecision;
use orca_core::config::RunConfig;
use orca_core::tool_types::{ToolCapability, ToolName, ToolRequest};
use orca_mcp::McpRegistry;

use crate::runtime_readonly_tool_turn::{collect_readonly_batch, should_run_readonly_batch};
use crate::step_context::{RuntimeSamplingRequestState, RuntimeToolDispatchWindow};
use crate::subagent_execution::{collect_subagent_batch, should_run_subagent_batch};
use crate::tool_invocation::{
    approval_request_for_invocation, prepare_tool_invocation_with_external,
    validate_tool_invocation,
};

pub(crate) enum RuntimeToolDispatch<'a> {
    Normal(&'a ToolRequest),
    ReadonlyBatch(RuntimeToolDispatchWindow<'a>),
    NetworkReadBatch(RuntimeToolDispatchWindow<'a>),
    SubagentBatch(RuntimeToolDispatchWindow<'a>),
}

pub(crate) struct NetworkReadScheduling<'a> {
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) sequential_until: &'a mut usize,
    pub(crate) tool_allowed: &'a dyn Fn(&ToolRequest) -> bool,
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
        network: Option<NetworkReadScheduling<'_>>,
    ) -> Option<RuntimeToolDispatch<'requests>> {
        let current = sampling_state.current_tool_request(tool_requests)?;

        if should_run_subagent_batch(self.config, current, self.subagent_depth) {
            let window = sampling_state.tool_dispatch_window(tool_requests, |requests, start| {
                collect_subagent_batch(self.config, requests, start)
            });
            return Some(RuntimeToolDispatch::SubagentBatch(window));
        }

        let max_parallel = self.config.tools.max_read_parallel;
        if let Some(network) = network {
            let window = sampling_state.tool_dispatch_window(tool_requests, |requests, start| {
                if start < *network.sequential_until || max_parallel <= 1 {
                    return start;
                }
                start
                    + requests[start..]
                        .iter()
                        .take(max_parallel)
                        .take_while(|request| {
                            (should_run_readonly_batch(max_parallel, request)
                                || is_network_read(request))
                                && (network.tool_allowed)(request)
                        })
                        .count()
            });
            let start = window.end_index() - window.tool_requests().len();
            if start < *network.sequential_until {
                return Some(RuntimeToolDispatch::Normal(current));
            }
            if window.tool_requests().len() > 1
                && window.tool_requests().iter().any(is_network_read)
            {
                if !sampling_state.permission_overlay.strict_auto_review()
                    && window
                        .tool_requests()
                        .iter()
                        .all(|request| self.allows_without_asking(request, &network))
                {
                    return Some(RuntimeToolDispatch::NetworkReadBatch(window));
                }
                *network.sequential_until = window.end_index();
                return Some(RuntimeToolDispatch::Normal(current));
            }
        }

        if should_run_readonly_batch(max_parallel, current) {
            let window = sampling_state.tool_dispatch_window(tool_requests, |requests, start| {
                collect_readonly_batch(self.config.tools.max_read_parallel, requests, start)
            });
            return Some(RuntimeToolDispatch::ReadonlyBatch(window));
        }

        Some(RuntimeToolDispatch::Normal(current))
    }

    fn allows_without_asking(
        &self,
        request: &ToolRequest,
        network: &NetworkReadScheduling<'_>,
    ) -> bool {
        let invocation = prepare_tool_invocation_with_external(
            request,
            self.subagent_depth,
            self.config.subagents.max_depth,
            network.mcp_registry,
            &self.config.external_tools,
        );
        if self.config.subagents.effective_definition.is_some()
            && validate_tool_invocation(&invocation, network.mcp_registry, self.config).is_err()
        {
            return false;
        }
        let Some(approval) = approval_request_for_invocation(&invocation) else {
            return false;
        };
        network
            .policy
            .resolve_for_tool(
                &approval,
                invocation.effective.name.as_str(),
                approval.target.as_deref().or(request.target.as_deref()),
            )
            .decision
            == ApprovalDecision::Allow
    }
}

pub(crate) fn is_network_read(request: &ToolRequest) -> bool {
    // These built-in reads can use the normal backend without an interactive worker.
    matches!(
        request.name,
        ToolName::WebSearch | ToolName::WebFetch | ToolName::ReadDocument
    ) && orca_tools::canonical_capabilities(request).is_some_and(|capabilities| {
        capabilities.contains(ToolCapability::NetworkSearch)
            && capabilities.is_read_only_or_network_search()
    })
}

#[cfg(test)]
mod tests {
    use orca_core::approval_rules::{PermissionRule, PermissionRules};
    use orca_core::approval_types::{ActionKind, ApprovalMode, Decision};
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
            startup_approval_mode: None,
            execution_profile: orca_core::capability::ExecutionProfile::Workspace,
            output_format: OutputFormat::Jsonl,
            verifier: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            theme: ThemeName::Dark,
            mcp_servers: Vec::new(),
            mcp_credentials_path: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            hooks: Vec::new(),
            workflows: WorkflowConfig::default(),
            subagents: SubagentConfig {
                max_depth: 1,
                limits: orca_core::subagent_config::SubagentLimits {
                    max_running: 3,
                    ..Default::default()
                },
                ..SubagentConfig::default()
            },
            computer_use: orca_core::config::ComputerUseConfig::default(),
            tools: ToolConfig {
                max_read_parallel: 3,
                ..ToolConfig::default()
            },
            external_tools: Vec::new(),
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            pricing_refresh: false,
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
                    "prompt": format!("inspect {id}"), "mode": "sync"
                })
                .to_string(),
            ),
        }
    }

    fn ids<'a>(dispatch: &'a RuntimeToolDispatch<'a>) -> Vec<&'a str> {
        match dispatch {
            RuntimeToolDispatch::Normal(request) => vec![request.id.as_str()],
            RuntimeToolDispatch::ReadonlyBatch(window)
            | RuntimeToolDispatch::NetworkReadBatch(window)
            | RuntimeToolDispatch::SubagentBatch(window) => window
                .tool_requests()
                .iter()
                .map(|request| request.id.as_str())
                .collect(),
        }
    }

    #[test]
    fn scheduler_keeps_a_network_read_forbidden_by_the_frozen_agent_out_of_the_batch() {
        let mut config = config();
        config.approval_mode = ApprovalMode::FullAuto;
        config.subagents.effective_definition = Some(
            orca_core::subagent_types::agent_definition::EffectiveAgentDefinition {
                name: "review".to_string(),
                description: "Review local files".to_string(),
                system_prompt: "Inspect the workspace".to_string(),
                allowed_tools: vec!["read_file".to_string()],
                model: None,
            },
        );
        let fetch = |id| ToolRequest {
            raw_arguments: Some(json!({"url": "https://example.com/"}).to_string()),
            ..request(id, ToolName::WebFetch, ActionKind::Network)
        };
        let requests = [fetch("first"), fetch("second")];
        let scheduler = RuntimeToolDispatchScheduler::new(&config, 1);
        let policy = crate::tool_execution::policy_for_tool_execution(&config);
        let dispatch = scheduler.next_dispatch(
            &RuntimeSamplingRequestState::new(),
            &requests,
            Some(NetworkReadScheduling {
                policy: &policy,
                mcp_registry: &McpRegistry::default(),
                sequential_until: &mut 0,
                tool_allowed: &|_| true,
            }),
        );
        assert!(
            matches!(dispatch, Some(RuntimeToolDispatch::Normal(request)) if request.id == "first")
        );
    }

    #[test]
    fn scheduler_uses_permission_rules_for_the_whole_network_window() {
        let mut config = config();
        let search = |id: &str, query: &str| ToolRequest {
            raw_arguments: Some(json!({"query": query}).to_string()),
            ..request(id, ToolName::WebSearch, ActionKind::Network)
        };
        let requests = [
            search("first", "review"),
            request("local", ToolName::ReadFile, ActionKind::Read),
            search("last", "reference"),
        ];
        let registry = McpRegistry::default();
        config.permission_rules.rules = vec![
            PermissionRule::new("web_search", "review", Decision::Allow),
            PermissionRule::new("web_search", "reference", Decision::Allow),
        ];
        for allow_last in [true, false] {
            config.permission_rules.rules[1].decision = if allow_last {
                Decision::Allow
            } else {
                Decision::Prompt
            };
            let scheduler = RuntimeToolDispatchScheduler::new(&config, 0);
            let policy = crate::tool_execution::policy_for_tool_execution(&config);
            let mut state = RuntimeSamplingRequestState::new();
            let mut sequential_until = 0;
            let mut dispatch = |state: &RuntimeSamplingRequestState| {
                scheduler
                    .next_dispatch(
                        state,
                        &requests,
                        Some(NetworkReadScheduling {
                            policy: &policy,
                            mcp_registry: &registry,
                            sequential_until: &mut sequential_until,
                            tool_allowed: &|_| true,
                        }),
                    )
                    .unwrap()
            };
            if allow_last {
                let batch = dispatch(&state);
                assert!(matches!(batch, RuntimeToolDispatch::NetworkReadBatch(_)));
                assert_eq!(ids(&batch), ["first", "local", "last"]);
            } else {
                for id in ["first", "local", "last"] {
                    assert!(
                        matches!(dispatch(&state), RuntimeToolDispatch::Normal(request) if request.id == id)
                    );
                    state.advance_tool_cursor_one(requests.len());
                }
            }
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
            .next_dispatch(&sampling_state, &requests, None)
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
            .next_dispatch(&sampling_state, &requests, None)
            .expect("first dispatch");
        assert!(matches!(first, RuntimeToolDispatch::ReadonlyBatch(_)));
        assert_eq!(ids(&first), vec!["read-a"]);

        sampling_state.advance_tool_cursor_one(requests.len());
        let second = RuntimeToolDispatchScheduler::new(&config, 0)
            .next_dispatch(&sampling_state, &requests, None)
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
            .next_dispatch(&sampling_state, &requests, None)
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
            .next_dispatch(&sampling_state, &requests, None)
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
                .next_dispatch(&sampling_state, &requests, None)
                .is_none()
        );
    }
}
