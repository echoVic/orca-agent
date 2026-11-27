use std::collections::HashMap;
use std::path::Path;

use orca_core::conversation::{Conversation, Message, ProviderForkPrefix};
use serde::Deserialize;

#[derive(Deserialize)]
struct WireMessage<'a> {
    role: &'a str,
    #[serde(default)]
    content: serde_json::Value,
}

pub(crate) fn sources_can_replay(prefix: &ProviderForkPrefix, cwd: &Path) -> bool {
    let mut loaded_sources = HashMap::new();
    for metadata in &prefix.system_messages {
        if metadata.system_prompt.as_ref().is_some_and(|record| {
            !crate::history::prompt_sources_can_replay(record, cwd, &mut loaded_sources)
        }) || !crate::history::instruction_fragments_can_replay(
            &Message::context_fragment_batch(String::new(), metadata.context_fragments.clone()),
            cwd,
        ) {
            return false;
        }
    }
    prefix.messages.iter().enumerate().all(|(index, wire)| {
        let Ok(message) = serde_json::from_str::<WireMessage<'_>>(wire) else {
            return false;
        };
        let Some(content) = message.content.as_str() else {
            return true;
        };
        let message = match message.role {
            "system" => {
                // An older unscoped prompt can carry workspace text without its sources.
                if content.contains("<project-instructions>")
                    && !prefix
                        .system_messages
                        .iter()
                        .any(|message| message.index == index)
                {
                    return false;
                }
                Message::system(content.into())
            }
            "user" if content.starts_with(orca_core::conversation::SKILL_INVOCATION_PREFIX) => {
                Message::pinned_user(content.into())
            }
            _ => return true,
        };
        crate::skill_context::should_replay_mention(&message, cwd) != Some(false)
    })
}

pub(crate) fn refresh_prompt_sections(conversation: &mut Conversation) {
    let Some(prefix) = &conversation.fork_prefix else {
        return;
    };
    let mut recorded = prefix.system_messages.iter().filter_map(|message| {
        let record = message.system_prompt.as_deref()?;
        let wire =
            serde_json::from_str::<WireMessage<'_>>(prefix.messages.get(message.index)?).ok()?;
        Some(Message::system_with_prompt_record(
            wire.content.as_str()?.into(),
            record.clone(),
        ))
    });
    let Some(leading) = recorded.next() else {
        return;
    };
    let record = leading.system_prompt_record().unwrap();
    if record.redacted || !crate::history::leading_prompt_matches_sections(&leading, record) {
        return;
    }
    let Some(cwd) = record
        .sections
        .iter()
        .find_map(|section| match section.name.as_str() {
            "cwd" => section
                .text
                .as_deref()?
                .strip_prefix("- Working directory: ")?
                .strip_suffix('\n')
                .map(Path::new),
            _ => None,
        })
    else {
        return;
    };
    // Rebuild the shared prompt sections in the original workspace. The
    // parent's role, mode and memory remain as recorded in the inherited brief.
    let shell_hidden = record.sections.iter().any(|section| {
        section.name == "environment"
            && section
                .text
                .as_deref()
                .is_some_and(|text| text.contains("Shell tool: unavailable for this role"))
    });
    let mut current =
        crate::system_prompt::build_system_prompt_sections(cwd, shell_hidden.then_some(&[][..]));
    let instructions = crate::instructions::load_for_cwd_or_default(cwd);
    if let Some(text) = instructions.to_system_prompt_block() {
        current.push_instructions(format!("\n\n{text}"), instructions.scope(cwd));
    }
    let current = current.into_message();
    if conversation.messages.iter().any(|message| {
        message
            .system_prompt_record()
            .is_some_and(|record| record.update && record.redacted)
    }) {
        return;
    }
    let mut history = vec![leading];
    history.extend(recorded);
    history.extend(
        conversation
            .messages
            .iter()
            .filter(|message| message.is_system_prompt_update())
            .cloned(),
    );
    conversation
        .messages
        .extend(crate::system_prompt::section_updates_matching(
            &history,
            &current,
            |name| {
                matches!(
                    name,
                    "identity" | "cwd" | "environment" | "agent behavior" | "project instructions"
                )
            },
        ));
}

pub(crate) fn redact_for_storage(prefix: &mut ProviderForkPrefix) -> serde_json::Result<()> {
    use crate::thread_store::{
        redact_fragment_metadata, redact_json_value, redact_prompt_record, redact_sensitive_text,
    };

    let mut metadata = prefix.system_messages.iter_mut().peekable();
    for (index, wire) in prefix.messages.iter_mut().enumerate() {
        let original: serde_json::Value = serde_json::from_str(wire)?;
        let mut saved = original.clone();
        redact_json_value(&mut saved);
        if let Some(system) = metadata.next_if(|system| system.index == index) {
            let before = original["content"].as_str().unwrap_or_default();
            let after = saved["content"].as_str().unwrap_or_default();
            if before != after {
                redact_fragment_metadata(before, after, &mut system.context_fragments);
            }
            if let Some(record) = system.system_prompt.as_mut() {
                redact_prompt_record(record, before != after);
            }
        }
        if original != saved {
            *wire = saved.to_string();
            prefix.redacted = true;
        }
    }
    let endpoint = redact_sensitive_text(&prefix.endpoint);
    prefix.redacted |= endpoint != prefix.endpoint;
    prefix.endpoint = endpoint;
    for tool in &mut prefix.tools {
        let original = tool.clone();
        redact_json_value(tool);
        prefix.redacted |= *tool != original;
    }
    Ok(())
}
