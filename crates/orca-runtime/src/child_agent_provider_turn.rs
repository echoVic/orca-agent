use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::model::ModelRouteContext;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::subagent_types::SubagentType;
use orca_provider::ProviderConfig;

use crate::child_agent_loop_setup::ChildAgentLoopSetup;
use crate::child_agent_types::{
    ChildAgentActivity, ChildAgentActivityObserver, ChildAgentRequest, ChildAgentResult,
};
use crate::compaction::{
    RuntimeCompactionPolicy, RuntimeCompactionRetryDecision, RuntimeCompactionStep,
};
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use crate::lifecycle::RuntimeTurnContext;

#[derive(Debug)]
pub enum ChildAgentProviderErrorDecision {
    RetryAfterCompaction,
    Fail(ChildAgentResult),
}

pub enum ChildAgentProviderTurn {
    Response(ProviderResponse),
    Fail {
        result: ChildAgentResult,
        usage: Option<orca_core::provider_types::Usage>,
    },
}

pub fn route_child_agent_model(
    config: &RunConfig,
    request: &ChildAgentRequest,
    setup: &ChildAgentLoopSetup,
    child_cost_tracker: &mut CostTracker,
) -> ProviderConfig {
    let route_decision = config.model.route(ModelRouteContext {
        subagent_type: &request.subagent_type,
        subagent_model: None,
    });
    child_cost_tracker.set_model(Some(&route_decision.actual_model));
    let mut provider_config = setup.provider_config.clone();
    provider_config.model = Some(route_decision.actual_model);
    provider_config
}

pub fn run_child_agent_provider_turn(
    config: &RunConfig,
    setup: &ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
    provider_config: &ProviderConfig,
    cancel: &CancelToken,
) -> ChildAgentProviderTurn {
    let pre_model_outcome = match hooks.run(
        orca_core::hook_types::HookEvent::PreModelCall,
        HookContext {
            cwd: &cwd.display().to_string(),
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            return ChildAgentProviderTurn::Fail {
                result: ChildAgentResult {
                    status: RunStatus::Failed,
                    final_message: None,
                    error: Some(format!("pre_model_call hook failed: {error}")),
                    budget_usage: None,
                },
                usage: None,
            };
        }
    };
    let model_conversation =
        conversation_with_hook_context(&setup.conversation, &pre_model_outcome);

    let response = orca_provider::call_streaming(
        config.provider,
        &model_conversation,
        provider_config,
        cancel,
        &mut |_| {},
    );

    if let Err(error) = hooks.run(
        orca_core::hook_types::HookEvent::PostModelCall,
        HookContext {
            cwd: &cwd.display().to_string(),
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: response.usage.as_ref(),
        },
    ) {
        return ChildAgentProviderTurn::Fail {
            result: ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(format!("post_model_call hook failed: {error}")),
                budget_usage: None,
            },
            usage: response.usage,
        };
    }

    ChildAgentProviderTurn::Response(response)
}

pub fn run_child_agent_provider_turn_observed(
    config: &RunConfig,
    setup: &ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
    provider_config: &ProviderConfig,
    cancel: &CancelToken,
    observer: Option<&ChildAgentActivityObserver<'_>>,
) -> ChildAgentProviderTurn {
    let pre_model_outcome = match hooks.run(
        orca_core::hook_types::HookEvent::PreModelCall,
        HookContext {
            cwd: &cwd.display().to_string(),
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            return ChildAgentProviderTurn::Fail {
                result: ChildAgentResult {
                    status: RunStatus::Failed,
                    final_message: None,
                    error: Some(format!("pre_model_call hook failed: {error}")),
                    budget_usage: None,
                },
                usage: None,
            };
        }
    };
    let model_conversation =
        conversation_with_hook_context(&setup.conversation, &pre_model_outcome);

    let response = orca_provider::call_streaming(
        config.provider,
        &model_conversation,
        provider_config,
        cancel,
        &mut |_| {
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::Streaming);
            }
        },
    );

    if let Err(error) = hooks.run(
        orca_core::hook_types::HookEvent::PostModelCall,
        HookContext {
            cwd: &cwd.display().to_string(),
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: response.usage.as_ref(),
        },
    ) {
        return ChildAgentProviderTurn::Fail {
            result: ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(format!("post_model_call hook failed: {error}")),
                budget_usage: None,
            },
            usage: response.usage,
        };
    }

    ChildAgentProviderTurn::Response(response)
}

pub fn compact_child_agent_conversation_if_needed(
    config: &RunConfig,
    setup: &mut ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
) -> io::Result<bool> {
    let mut events = EventFactory::new("child-agent-compaction".to_string());
    let mut sink = EventSink::new(io::sink(), config.output_format);
    let subagent_type = SubagentType::General;
    let mut compaction = RuntimeCompactionStep::new(
        config.provider,
        &setup.context_config,
        &setup.provider_config,
        RuntimeTurnContext::new(cwd, "", 0, false, &subagent_type),
        hooks,
        &mut events,
        &mut sink,
        None,
    );
    compaction.compact_if_needed(&mut setup.conversation)
}

pub fn handle_child_agent_provider_error(
    config: &RunConfig,
    setup: &mut ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
    response: &ProviderResponse,
) -> io::Result<Option<ChildAgentProviderErrorDecision>> {
    let Some(error) = response.steps.iter().find_map(|step| match step {
        ProviderStep::Error(message) => Some(message.clone()),
        _ => None,
    }) else {
        setup.compaction_retry.reset();
        return Ok(None);
    };

    match RuntimeCompactionPolicy::decide_for_provider_error(
        &error.message,
        &setup.compaction_retry,
    ) {
        RuntimeCompactionRetryDecision::CompactAndRetry { trigger, reason: _ } => {
            let mut events = EventFactory::new("child-agent-compaction".to_string());
            let mut sink = EventSink::new(io::sink(), config.output_format);
            let subagent_type = SubagentType::General;
            let mut compaction = RuntimeCompactionStep::new(
                config.provider,
                &setup.context_config,
                &setup.provider_config,
                RuntimeTurnContext::new(cwd, "", 0, false, &subagent_type),
                hooks,
                &mut events,
                &mut sink,
                None,
            );
            compaction.compact_after_provider_error_retry(&mut setup.conversation, trigger)?;
            setup.compaction_retry.record_prompt_too_long_retry();
            Ok(Some(ChildAgentProviderErrorDecision::RetryAfterCompaction))
        }
        RuntimeCompactionRetryDecision::SurfaceError => Ok(Some(
            ChildAgentProviderErrorDecision::Fail(ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(error.message),
                budget_usage: None,
            }),
        )),
    }
}
