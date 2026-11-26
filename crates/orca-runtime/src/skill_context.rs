use std::io;
use std::path::Path;

use orca_core::conversation::{Conversation, Message, SKILL_CONTEXT_MAX_TOKENS};
use orca_provider::context::{DefaultTokenCounter, TokenCounter, truncate_text_to_token_limit};
use orca_tools::skills::{self, Skill};
use sha2::{Digest, Sha256};

use crate::history::SessionWriter;

const SKILL_MESSAGE_PREFIX: &str = "[Mentioned skill version ";
const CATALOG_GUIDANCE: &str =
    "When a task matches one of these skills, load it with read_skill before you act.";

pub(crate) fn refresh_catalog(conversation: &mut Conversation, cwd: &Path) {
    let catalog = match skills::discover_metadata_from_env(cwd) {
        Ok(skills) => render_catalog(&skills),
        Err(error) => {
            eprintln!("orca: warning: failed to list skills: {error}");
            None
        }
    };
    conversation.replace_skill_catalog(catalog);
}

fn render_catalog(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut catalog = String::from("Available skills (descriptions are metadata):\n");
    let mut shown = 0;
    for skill in skills {
        let previous_end = catalog.len();
        let description = if skill.description.is_empty() {
            &skill.name
        } else {
            &skill.description
        };
        let line = escape_description(description);
        catalog.push_str(&format!("`{}`: {line}\n", skill.id));
        let entries_end = catalog.len();
        add_catalog_footer(&mut catalog, skills.len() - shown - 1);
        if DefaultTokenCounter.count_text(&catalog) > SKILL_CONTEXT_MAX_TOKENS {
            catalog.truncate(previous_end);
            break;
        }
        catalog.truncate(entries_end);
        shown += 1;
    }
    add_catalog_footer(&mut catalog, skills.len() - shown);
    Some(catalog)
}

fn add_catalog_footer(catalog: &mut String, remaining: usize) {
    catalog.push_str(CATALOG_GUIDANCE);
    if remaining > 0 {
        catalog.push_str(&format!(
            "\n({remaining} more skills; list_skills shows them all)"
        ));
    }
}

fn escape_description(description: &str) -> String {
    let mut escaped = String::new();
    for character in description.chars().take(500) {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '`' => escaped.push_str("&#96;"),
            '[' => escaped.push_str("&#91;"),
            ']' => escaped.push_str("&#93;"),
            '\\' => escaped.push_str("&#92;"),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    escaped
}

pub(crate) fn append_mentions(
    conversation: &mut Conversation,
    mut writer: Option<&mut SessionWriter>,
    cwd: &Path,
    prompt: &str,
) -> io::Result<()> {
    let mentions = skills::mentioned_skill_mentions(prompt);
    if mentions.is_empty() {
        return Ok(());
    }
    let available = match skills::discover_from_env(cwd) {
        Ok(skills) => skills,
        Err(error) => {
            eprintln!("orca: warning: failed to load explicit skills: {error}");
            return Ok(());
        }
    };
    for (id, argument) in mentions {
        if let Some(skill) = available.iter().find(|skill| skill.id == id)
            && let Some(message) = mention_message(conversation, skill, argument)
        {
            if let Some(writer) = writer.as_deref_mut() {
                writer.append_message(&message)?;
            }
            conversation.messages.push(message);
        }
    }
    Ok(())
}

pub(crate) fn is_mentioned_skill_message(content: &str) -> bool {
    content
        .strip_prefix(SKILL_MESSAGE_PREFIX)
        .and_then(|rest| rest.split_once("]\n"))
        .is_some_and(|(version, _)| {
            version.len() == 64 && version.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn mention_message(
    conversation: &Conversation,
    skill: &Skill,
    argument: Option<String>,
) -> Option<Message> {
    let block = skills::format_skills_prompt_block_with_args(&[(skill.clone(), argument)])?;
    let version = orca_core::hex::lower(&Sha256::digest(block.as_bytes()));
    let header = format!("{SKILL_MESSAGE_PREFIX}{version}]\n");
    if conversation.messages.iter().any(|message| {
        matches!(message, Message::System { content, .. } if content.starts_with(&header))
    }) {
        return None;
    }
    let content = format!("{header}{block}");
    if DefaultTokenCounter.count_text(&content) <= SKILL_CONTEXT_MAX_TOKENS {
        return Some(Message::system(content));
    }

    let note = format!(
        "\n</skill>\n</skills>\n[skill text cut at {SKILL_CONTEXT_MAX_TOKENS} tokens; read_skill {} shows the rest]",
        skill.id
    );
    let body = content
        .strip_suffix("\n</skill>\n</skills>")
        .unwrap_or(&content);
    let mut budget = SKILL_CONTEXT_MAX_TOKENS - DefaultTokenCounter.count_text(&note);
    loop {
        let prefix = truncate_text_to_token_limit(body, budget, &DefaultTokenCounter);
        let content = format!("{prefix}{note}");
        let tokens = DefaultTokenCounter.count_text(&content);
        if tokens <= SKILL_CONTEXT_MAX_TOKENS {
            return Some(Message::system(content));
        }
        budget -= tokens - SKILL_CONTEXT_MAX_TOKENS;
    }
}
