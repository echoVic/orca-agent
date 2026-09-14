use orca_core::budget::BudgetUsage;
use orca_core::config::RunConfig;
use orca_core::conversation::{Conversation, Message};
use orca_core::subagent_types::SubagentType;
use orca_provider::ProviderConfig;
use std::time::Duration;

pub(crate) const FINAL_REPORT_PROMPT: &str = "[Read-only investigation finalization]\nThe runtime evidence-gathering budget is complete. Do not call tools. Return the best concise final report you can from evidence already gathered. Include the direct result, exact path:line evidence for material claims, and unresolved questions. Partial findings are acceptable; do not broaden the investigation.";
pub(crate) const FINAL_REPORT_MAX_TOKENS: u32 = 1_024;
pub(crate) const FINAL_REPORT_TIMEOUT: Duration = Duration::from_secs(90);

pub(crate) fn applies(subagent_type: &SubagentType) -> bool {
    subagent_type
        .builtin()
        .is_some_and(|descriptor| descriptor.is_read_only())
}

pub(crate) fn summary_due(
    config: &RunConfig,
    subagent_type: &SubagentType,
    usage: BudgetUsage,
) -> bool {
    applies(subagent_type)
        && (usage.turns >= config.subagents.max_investigation_turns.max(1)
            || usage.tool_calls >= config.subagents.max_investigation_tool_calls.max(1))
}

pub(crate) fn summary_prompt_present(conversation: &Conversation) -> bool {
    conversation.messages.iter().any(|message| {
        matches!(message, Message::System { content, .. } | Message::User { content, .. }
            if content == FINAL_REPORT_PROMPT)
    })
}

/// Returns true only when the user explicitly made both halves of a read-only
/// contract clear: this is an investigation and repository mutation is
/// forbidden. Requiring both signals keeps implementation tasks free to use
/// child research before they begin editing.
pub(crate) fn explicit_read_only_request(conversation: &Conversation) -> bool {
    let Some(prompt) = conversation
        .messages
        .iter()
        .find_map(|message| match message {
            Message::User { content, .. } if content != FINAL_REPORT_PROMPT => Some(content),
            _ => None,
        })
    else {
        return false;
    };
    let prompt = prompt.to_lowercase();
    let investigation =
        prompt.contains("read-only") || prompt.contains("read only") || prompt.contains("只读");
    let no_mutation = prompt.contains("do not edit")
        || prompt.contains("don't edit")
        || prompt.contains("no edits")
        || prompt.contains("不要修改")
        || prompt.contains("不修改")
        || prompt.contains("禁止修改");
    investigation && no_mutation
}

pub(crate) fn ensure_summary_prompt(conversation: &mut Conversation) -> bool {
    if summary_prompt_present(conversation) {
        return false;
    }
    conversation.add_user_pinned(FINAL_REPORT_PROMPT.to_string());
    true
}

pub(crate) fn disable_tools(provider_config: &mut ProviderConfig) {
    provider_config.tools_override = Some(Vec::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convergence_only_applies_to_builtin_read_only_roles() {
        assert!(applies(&SubagentType::Explorer));
        assert!(applies(&SubagentType::CodeReviewer));
        assert!(!applies(&SubagentType::General));
        assert!(!applies(&SubagentType::Custom("readonly".to_string())));
    }

    #[test]
    fn finalization_prompt_is_idempotent() {
        let mut conversation = Conversation::new();
        assert!(ensure_summary_prompt(&mut conversation));
        assert!(!ensure_summary_prompt(&mut conversation));
        assert_eq!(conversation.messages.len(), 1);
        assert!(summary_prompt_present(&conversation));
    }

    #[test]
    fn delegated_parent_convergence_requires_an_explicit_read_only_contract() {
        let mut conversation = Conversation::new();
        conversation.add_user(
            "Read-only architecture review. Return evidence and do not edit files.".to_string(),
        );
        assert!(explicit_read_only_request(&conversation));

        let mut implementation = Conversation::new();
        implementation.add_user(
            "Ask explorers to inspect the architecture, then edit the implementation.".to_string(),
        );
        assert!(!explicit_read_only_request(&implementation));
    }
}
