use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::folder_trust::is_trusted;
use orca_core::conversation::{
    Conversation, InstructionScope, Message, SKILL_CONTEXT_MAX_TOKENS, SKILL_INVOCATION_PREFIX,
    SKILLS_CONTEXT_FRAGMENT_ID,
};
use orca_provider::context::{DefaultTokenCounter, TokenCounter, truncate_text_to_token_limit};
use orca_tools::skills::{self, Skill, SkillSource};
use sha2::{Digest, Sha256};

use crate::history::SessionWriter;

const SKILL_MESSAGE_PREFIX: &str = "[Mentioned skill version ";
const CATALOG_GUIDANCE: &str =
    "When a task matches one of these skills, load it with read_skill before you act.";

pub(crate) fn refresh_catalog(conversation: &mut Conversation, cwd: &Path) {
    match skills::discover_metadata_from_env(cwd) {
        Ok(skills) => {
            conversation.replace_skill_catalog(render_catalog(&skills));
            conversation.internal_context.set_instruction_scope(
                SKILLS_CONTEXT_FRAGMENT_ID,
                InstructionScope {
                    cwd: cwd.to_owned(),
                    sources: project_skill_paths(&skills).cloned().collect(),
                },
            );
        }
        Err(error) => {
            eprintln!("orca: warning: failed to list skills: {error}");
            conversation.replace_skill_catalog(None);
        }
    }
}

fn project_skill_paths(skills: &[Skill]) -> impl Iterator<Item = &PathBuf> {
    skills
        .iter()
        .filter(|skill| skill.source == SkillSource::Project)
        .map(|skill| &skill.path)
}

pub(crate) fn catalog_scope_is_loaded(scope: &InstructionScope, cwd: &Path) -> bool {
    scope.sources.is_empty()
        || (scope.cwd == cwd
            && skills::discover_metadata_from_env(cwd).is_ok_and(|skills| {
                let loaded = project_skill_paths(&skills).collect::<std::collections::HashSet<_>>();
                scope.sources.iter().all(|path| loaded.contains(path))
            }))
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
        let Some(skill) = available.iter().find(|skill| skill.id == id) else {
            continue;
        };
        let version = skill_version(skill, cwd);
        let mut messages = Vec::new();
        if let Some(message) = mention_message(conversation, skill, &version) {
            messages.push(message);
        }
        if let Some(argument) = argument.filter(|_| skill.body.contains("{{arg}}")) {
            let header = format!("{SKILL_INVOCATION_PREFIX}{version}]\n");
            let expanded = skill.body.replace("{{arg}}", &argument);
            let budget =
                SKILL_CONTEXT_MAX_TOKENS.saturating_sub(DefaultTokenCounter.count_text(&header));
            let body = truncate_text_to_token_limit(&expanded, budget, &DefaultTokenCounter);
            messages.push(Message::pinned_user(format!("{header}{body}")));
        }
        for message in messages {
            if let Some(writer) = writer.as_deref_mut() {
                writer.append_message(&message)?;
            }
            conversation.messages.push(message);
        }
    }
    Ok(())
}

pub(crate) fn should_replay_mention(message: &Message, cwd: &Path) -> Option<bool> {
    let version = match message {
        Message::System { content, .. } => content.strip_prefix(SKILL_MESSAGE_PREFIX),
        Message::User {
            content,
            pinned: true,
            ..
        } => content.strip_prefix(SKILL_INVOCATION_PREFIX),
        _ => None,
    }?;
    // Older mention records lack provenance and can contain expanded arguments.
    Some(
        version
            .split_once("]\n")
            .and_then(|(version, _)| version.split_once(' '))
            .and_then(|(_, root)| serde_json::from_str::<Option<PathBuf>>(root).ok())
            .is_some_and(|root| root.is_none_or(|root| is_trusted(&root) && is_trusted(cwd))),
    )
}

fn skill_version(skill: &Skill, cwd: &Path) -> String {
    let root = (skill.source == SkillSource::Project).then(|| skills::project_skill_root(cwd));
    let path = skill.path.to_string_lossy();
    let source = serde_json::to_vec(&(
        &skill.id,
        &skill.name,
        &path,
        &skill.when_to_use,
        &skill.body,
    ))
    .expect("skill source is serializable");
    let version = orca_core::hex::lower(&Sha256::digest(source));
    let root = serde_json::to_string(&root.as_deref().map(Path::to_string_lossy))
        .expect("skill root is serializable");
    format!("{version} {root}")
}

fn mention_message(conversation: &Conversation, skill: &Skill, version: &str) -> Option<Message> {
    let block = skills::format_skills_prompt_block(std::slice::from_ref(skill))?;
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
