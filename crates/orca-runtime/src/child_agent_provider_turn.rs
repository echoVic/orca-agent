use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::model::{ImageRouteDecision, ModelRouteContext};
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::subagent_types::SubagentType;
use orca_core::tool_images::{TOOL_IMAGE_UNAVAILABLE_NOTE, drop_tool_images};

use crate::child_agent_loop_setup::ChildAgentLoopSetup;
use crate::child_agent_types::{
    ChildAgentActivity, ChildAgentActivityPublisher, ChildAgentRequest, ChildAgentResult,
    child_event_output,
};
use crate::compaction::{
    RuntimeCompactionPolicy, RuntimeCompactionRetryDecision, RuntimeCompactionStep,
};
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookOutcome, HookRunner, conversation_with_hook_context};
use crate::image_routing::conversation_has_images;
use crate::lifecycle::{RuntimeModelTurn, RuntimeTurnContext};

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
) -> RuntimeModelTurn {
    let decision = config.model.route(ModelRouteContext {
        subagent_type: &request.subagent_type,
        subagent_model: None,
        has_images: conversation_has_images(&setup.conversation),
    });
    child_cost_tracker.set_model(Some(&decision.actual_model));
    let mut provider_config = setup.provider_config.clone();
    provider_config.model = Some(decision.actual_model.clone());
    RuntimeModelTurn {
        decision,
        provider_config,
    }
}

/// The conversation a child sends to its model: its own conversation plus the
/// pre-model hook context.
///
/// Child turns skip `prepare_image_conversation`, so unless the route is
/// `Direct`, this copy carries a note in place of each tool image. The child's
/// own conversation keeps its images.
pub(crate) fn child_agent_model_conversation(
    conversation: &Conversation,
    pre_model_outcome: &HookOutcome,
    image_route: ImageRouteDecision,
) -> Conversation {
    let mut model_conversation = conversation_with_hook_context(conversation, pre_model_outcome);
    if image_route != ImageRouteDecision::Direct {
        drop_tool_images(
            &mut model_conversation.messages,
            TOOL_IMAGE_UNAVAILABLE_NOTE,
        );
    }
    model_conversation
}

pub fn run_child_agent_provider_turn(
    config: &RunConfig,
    setup: &ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
    model_turn: &RuntimeModelTurn,
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
    let model_conversation = child_agent_model_conversation(
        &setup.conversation,
        &pre_model_outcome,
        model_turn.decision.image_route,
    );

    let response = orca_provider::call_streaming(
        config.provider,
        &model_conversation,
        &model_turn.provider_config,
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
    model_turn: &RuntimeModelTurn,
    cancel: &CancelToken,
    observer: Option<&dyn ChildAgentActivityPublisher>,
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
    let model_conversation = child_agent_model_conversation(
        &setup.conversation,
        &pre_model_outcome,
        model_turn.decision.image_route,
    );

    let mut activity_error = None;
    let response = orca_provider::call_streaming(
        config.provider,
        &model_conversation,
        &model_turn.provider_config,
        cancel,
        &mut |_| {
            if let Some(observer) = observer {
                if let Err(error) = observer.publish_activity(ChildAgentActivity::Streaming) {
                    activity_error = Some(error.to_string());
                }
            }
        },
    );

    if let Some(error) = activity_error {
        return ChildAgentProviderTurn::Fail {
            result: ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(format!("child activity sink failed: {error}")),
                budget_usage: None,
            },
            usage: response.usage,
        };
    }

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
    let mut sink = EventSink::new(child_event_output(), config.output_format);
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

/// Before a child request: compact when needed, and fail the child with a
/// clear message when not even emergency compaction leaves room to reply.
/// Only the child fails; its parent session can go on.
pub fn prepare_child_agent_request(
    config: &RunConfig,
    setup: &mut ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
) -> io::Result<Result<(), String>> {
    let mut events = EventFactory::new("child-agent-compaction".to_string());
    let mut sink = EventSink::new(child_event_output(), config.output_format);
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
    Ok(compaction
        .prepare_request(&mut setup.conversation)?
        .map_err(|overflow| {
            crate::compaction::child_context_overflow_message(overflow.prompt_tokens)
        }))
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
            let mut sink = EventSink::new(child_event_output(), config.output_format);
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
            if compaction.compact_after_provider_error_retry(&mut setup.conversation, trigger)? {
                setup.compaction_retry.record_prompt_too_long_retry();
                Ok(Some(ChildAgentProviderErrorDecision::RetryAfterCompaction))
            } else {
                Ok(Some(ChildAgentProviderErrorDecision::Fail(
                    ChildAgentResult {
                        status: RunStatus::Failed,
                        final_message: None,
                        error: Some(crate::compaction::child_unrecoverable_overflow_message(
                            &error.message,
                        )),
                        budget_usage: None,
                    },
                )))
            }
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
