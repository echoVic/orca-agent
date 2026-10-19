use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::RunStatus;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::context::ContextConfig;

use crate::agent_common;
use crate::agent_continuation::{
    AgentAttemptId, AgentContinuationError, AgentContinuationId, AgentPromptId,
};
use crate::budget_controller::BudgetLease;
use crate::child_agent_types::{ChildAgentContinuationStart, ChildAgentRequest, ChildAgentResult};
use crate::compaction::RuntimeCompactionRetryState;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub struct ChildAgentLoopSetup {
    pub mcp_registry: McpRegistry,
    pub provider_config: ProviderConfig,
    pub context_config: ContextConfig,
    pub conversation: Conversation,
    pub policy: ApprovalPolicy,
    pub(crate) continuation: Option<ChildAgentContinuationRuntimeState>,
    pub(crate) turn: u32,
    pub(crate) compaction_retry: RuntimeCompactionRetryState,
}

pub(crate) struct PreparedChildAgentConversation {
    pub(crate) conversation: Conversation,
    pub(crate) continuation: Option<ChildAgentContinuationRuntimeState>,
    pub(crate) turn: u32,
}

/// Resume-only loop state retained until the setup stage restores the durable
/// conversation and establishes its next-turn cursor.
#[derive(Clone, Debug)]
pub(crate) struct ChildAgentContinuationRuntimeState {
    pub(crate) start: ChildAgentContinuationStart,
    pub(crate) restored_next_turn: Option<u32>,
}

pub enum ChildAgentTurnBudget {
    Continue,
    Stop(ChildAgentResult),
}

/// Builds the existing fresh child setup with exactly one current system
/// prompt, the request prompt as the first user message, and turn zero. This
/// compatibility entry point deliberately does not restore continuation data;
/// production loop runners use `try_prepare_child_agent_loop` for all requests.
pub fn prepare_child_agent_loop(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> ChildAgentLoopSetup {
    let conversation =
        prepare_fresh_child_agent_conversation(config, request, cwd, instructions, memory);
    build_child_agent_loop_setup(config, request, cwd, conversation, None, 0)
}

/// Builds a production child setup. Fresh requests are exactly equivalent to
/// `prepare_child_agent_loop`. Continuation requests create only the current
/// system prompt, restore the checkpoint's non-system state, append the new
/// user prompt, and return stable continuation errors without partial setup.
pub(crate) fn try_prepare_child_agent_loop(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> Result<ChildAgentLoopSetup, AgentContinuationError> {
    let prepared =
        try_prepare_child_agent_conversation(config, request, cwd, instructions, memory)?;
    Ok(build_child_agent_loop_setup(
        config,
        request,
        cwd,
        prepared.conversation,
        prepared.continuation,
        prepared.turn,
    ))
}

/// Prepares the child conversation independently from provider and MCP setup.
/// Fresh input receives the current system prompt plus request prompt; resume
/// input validates and restores the checkpoint before appending the prompt.
/// It returns the loop cursor/state or a stable continuation error and performs
/// no provider, MCP, persistence, or observer side effects.
pub(crate) fn try_prepare_child_agent_conversation(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> Result<PreparedChildAgentConversation, AgentContinuationError> {
    let Some(start) = request.continuation.clone() else {
        return Ok(PreparedChildAgentConversation {
            conversation: prepare_fresh_child_agent_conversation(
                config,
                request,
                cwd,
                instructions,
                memory,
            ),
            continuation: None,
            turn: 0,
        });
    };

    let mut conversation =
        prepare_child_agent_system_conversation(config, request, cwd, instructions, memory);
    let mut continuation = ChildAgentContinuationRuntimeState {
        start,
        restored_next_turn: None,
    };
    let next_turn = restore_child_agent_continuation(&mut conversation, &continuation.start)?;
    continuation.restored_next_turn = Some(next_turn);
    conversation.add_user(request.prompt.clone());
    let turn = next_turn
        .checked_sub(1)
        .ok_or_else(|| AgentContinuationError::CorruptRecord {
            message: "child continuation next turn must be at least one".to_string(),
        })?;
    Ok(PreparedChildAgentConversation {
        conversation,
        continuation: Some(continuation),
        turn,
    })
}

fn build_child_agent_loop_setup(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    conversation: Conversation,
    continuation: Option<ChildAgentContinuationRuntimeState>,
    turn: u32,
) -> ChildAgentLoopSetup {
    let mcp_registry =
        orca_mcp::initialize_registry(&config.mcp_servers, config.mcp_credentials_path.clone());
    mcp_registry.wait_for_startup(&|| false);
    let mut provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: crate::tool_invocation::provider_tool_schema_override(
            request.depth,
            &request.subagent_type,
            crate::tool_invocation::AgentToolPolicyContext::new(
                request.allowed_tools.as_deref(),
                request.tool_policy_label.as_deref(),
            ),
            &mcp_registry,
            &config.external_tools,
        ),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
        max_output_tokens: None,
    };
    if let Some(tools) = provider_config.tools_override.as_mut()
        && !crate::workspace_git::is_git_work_tree(cwd)
    {
        tools.retain(|tool| tool.name != "git_status");
    }

    let budget_model = config.model.as_option();
    let context_config = ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
        config.reasoning_effort,
    );
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());

    ChildAgentLoopSetup {
        mcp_registry,
        provider_config,
        context_config,
        conversation,
        policy,
        continuation,
        turn,
        compaction_retry: RuntimeCompactionRetryState::default(),
    }
}

fn prepare_fresh_child_agent_conversation(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> Conversation {
    let mut conversation =
        prepare_child_agent_system_conversation(config, request, cwd, instructions, memory);
    conversation.add_user(request.prompt.clone());
    conversation
}

fn prepare_child_agent_system_conversation(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> Conversation {
    let mut conversation = Conversation::new();
    let system_prompt = agent_common::build_agent_system_prompt_for_config(
        cwd,
        config,
        request.depth,
        &request.subagent_type,
        Some(instructions),
        Some(memory),
        request.allowed_tools.as_deref(),
    );
    conversation.add_system(system_prompt);
    conversation
}

/// Restores one validated continuation checkpoint into a conversation that
/// contains only the freshly generated system prompt. The coordinator owns
/// lineage validation; this setup boundary revalidates all typed start ids,
/// the checkpoint digest, and the turn cursor. A checkpoint from the current
/// attempt or its `resumed_from` attempt is accepted, so attempt ids are not
/// required to be equal here.
fn restore_child_agent_continuation(
    conversation: &mut Conversation,
    start: &ChildAgentContinuationStart,
) -> Result<u32, AgentContinuationError> {
    AgentContinuationId::parse(start.continuation_id().as_str().to_string())?;
    AgentAttemptId::parse(start.attempt_id().as_str().to_string())?;
    AgentPromptId::parse(start.prompt_id().as_str().to_string())?;
    AgentAttemptId::parse(start.checkpoint().attempt_id.as_str().to_string())?;
    start.checkpoint().verify_digest()?;

    let expected_next_turn = start.checkpoint().turn.checked_add(1).ok_or_else(|| {
        AgentContinuationError::CorruptRecord {
            message: "child continuation turn cursor is exhausted".to_string(),
        }
    })?;
    if start.checkpoint().conversation.next_turn != expected_next_turn {
        return Err(AgentContinuationError::CorruptRecord {
            message: "child continuation next turn does not follow checkpoint turn".to_string(),
        });
    }

    start.checkpoint().conversation.restore_into(conversation)
}

/// Advances the child loop one turn through the child's budget lease. The
/// lease is bounded by the parent's remaining operation budget, so a child
/// can never spend beyond what the parent reserved for it.
pub fn advance_child_agent_turn(
    setup: &mut ChildAgentLoopSetup,
    lease: &mut BudgetLease,
) -> ChildAgentTurnBudget {
    setup.turn = setup.turn.saturating_add(1);
    if let Err(stop) = lease.admit_turn() {
        return ChildAgentTurnBudget::Stop(ChildAgentResult {
            status: RunStatus::Failed,
            final_message: None,
            error: Some(format!(
                "budget stopped: child {} (turns={}, tool_calls={})",
                stop.reason.as_str(),
                stop.usage.turns,
                stop.usage.tool_calls
            )),
            budget_usage: None,
        });
    }

    ChildAgentTurnBudget::Continue
}

/// Test-only variant that advances against an explicit turn ceiling without a
/// parent lease.
#[cfg(test)]
pub fn advance_child_agent_turn_with_limit(
    setup: &mut ChildAgentLoopSetup,
    max_turns: u32,
) -> ChildAgentTurnBudget {
    setup.turn = setup.turn.saturating_add(1);
    if setup.turn > max_turns {
        return ChildAgentTurnBudget::Stop(ChildAgentResult {
            status: RunStatus::Failed,
            final_message: None,
            error: Some("budget stopped: child turn budget exhausted".to_string()),
            budget_usage: None,
        });
    }

    ChildAgentTurnBudget::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::subagent_types::SubagentType;
    use orca_core::subagent_types::agent_definition::EffectiveAgentDefinition;

    fn computer_request() -> (RunConfig, ChildAgentRequest) {
        let allowed_tools = vec!["computer".to_string(), "read_file".to_string()];
        let mut config = RunConfig::default();
        config.computer_use.interactive = true;
        config.subagents.effective_definition = Some(EffectiveAgentDefinition {
            name: "desktop-audit".to_string(),
            description: "Audit source".to_string(),
            system_prompt: "Report source findings.".to_string(),
            allowed_tools: allowed_tools.clone(),
            model: None,
        });
        let request = ChildAgentRequest {
            prompt: "inspect source".to_string(),
            subagent_type: SubagentType::Custom("desktop-audit".to_string()),
            model: None,
            depth: 1,
            emit_deltas: false,
            allowed_tools: Some(allowed_tools),
            tool_policy_label: Some("custom agent desktop-audit".to_string()),
            workflow_ipc: None,
            continuation: None,
        };
        (config, request)
    }

    #[test]
    fn child_git_status_catalog_matches_its_own_cwd() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let non_repo = tempfile::tempdir().unwrap();
        assert!(
            non_repo
                .path()
                .ancestors()
                .all(|dir| !dir.join(".git").exists())
        );
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let (mut config, mut request) = computer_request();
        config.cwd = Some(repo.path().to_path_buf());
        config.subagents.effective_definition = None;
        request.subagent_type = SubagentType::General;
        request.allowed_tools = None;

        for (cwd, is_repo) in [(non_repo.path(), false), (repo.path(), true)] {
            let setup = prepare_child_agent_loop(
                &config,
                &request,
                cwd,
                &ProjectInstructions::default(),
                &MemoryBlock::default(),
            );
            let prompt = setup.conversation.messages[0].content_str().unwrap();
            assert_eq!(prompt.contains("### git_status\n"), is_repo);
            assert_eq!(prompt.contains("- Git repository: no\n"), !is_repo);
            assert_eq!(
                setup
                    .provider_config
                    .tools_override
                    .unwrap()
                    .iter()
                    .any(|tool| tool.name == "git_status"),
                is_repo
            );
        }
    }

    #[test]
    fn a_sync_custom_child_cannot_offer_computer_even_when_its_definition_lists_it() {
        let (config, request) = computer_request();
        let setup = prepare_child_agent_loop(
            &config,
            &request,
            Path::new("/repo"),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
        );
        let tools = setup.provider_config.tools_override.expect("child schemas");
        assert!(tools.iter().any(|tool| tool.name == "read_file"));
        assert!(!tools.iter().any(|tool| tool.name == "computer"));
    }

    #[test]
    fn a_sync_custom_child_prompt_does_not_advertise_computer() {
        let (config, request) = computer_request();
        let conversation = prepare_child_agent_system_conversation(
            &config,
            &request,
            Path::new("/repo"),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
        );
        let prompt = conversation.messages[0]
            .content_str()
            .expect("system prompt");
        assert!(prompt.contains("### read_file"));
        assert!(prompt.contains("Report source findings."));
        assert!(!prompt.contains("### computer"));
    }
}
