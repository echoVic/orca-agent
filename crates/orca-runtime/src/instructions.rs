use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::conversation::{Conversation, Message};
use sha2::{Digest, Sha256};

use crate::history::SessionWriter;

const INSTRUCTIONS_FILE: &str = "AGENTS.md";
const RULES_DIR: &str = ".orca/rules";
const CHAIN_BYTES: usize = 65_536;
const BUDGET_NOTE: &str = "\n\n[Instruction budget: ";
const ADDITIONAL: &str = "Additional instructions for ";
const UPDATED: &str = "Updated instructions for ";

#[derive(Clone, Debug, Default)]
pub struct ProjectInstructions {
    sections: Vec<InstructionSection>,
    trusted_workspace: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct InstructionSection {
    source: PathBuf,
    content: String,
}

impl ProjectInstructions {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn to_system_prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut block = String::from("<project-instructions>\n");
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                block.push('\n');
            }
            block.push_str("# ");
            block.push_str(&section.source.display().to_string());
            block.push('\n');
            block.push_str(section.content.trim());
            block.push('\n');
        }
        block.push_str("</project-instructions>");
        Some(block)
    }
}

pub fn load_for_cwd_or_default(cwd: &Path) -> ProjectInstructions {
    match load_for_cwd(cwd) {
        Ok(instructions) => instructions,
        Err(error) => {
            eprintln!("orca: warning: failed to load project instructions: {error}");
            ProjectInstructions::default()
        }
    }
}

pub fn load_for_cwd(cwd: &Path) -> io::Result<ProjectInstructions> {
    let orca_home_path = orca_core::home::orca_home();
    load_for_cwd_with_home(cwd, orca_home_path.as_deref())
}

fn load_for_cwd_with_home(
    cwd: &Path,
    orca_home_path: Option<&Path>,
) -> io::Result<ProjectInstructions> {
    let mut sections = Vec::new();
    let mut visited = HashSet::new();

    if let Some(user_agents) = orca_home_path.map(|home| home.join(INSTRUCTIONS_FILE))
        && user_agents.is_file()
    {
        sections.push(InstructionSection {
            source: user_agents.clone(),
            content: read_with_includes(&user_agents, &mut visited, orca_home_path)?,
        });
    }

    apply_prompt_budget(&mut sections);
    let mut project_sections = Vec::new();
    let is_trusted = |path: &Path| {
        orca_home_path.is_some_and(|home| {
            orca_core::config::folder_trust::is_trusted_with_config_dir(path, home)
        })
    };
    if let Some(project_root) = find_project_root(cwd).filter(|root| is_trusted(root)) {
        let root_agents = project_root.join(INSTRUCTIONS_FILE);
        if root_agents.is_file() {
            project_sections.push(InstructionSection {
                source: root_agents.clone(),
                content: read_with_includes(&root_agents, &mut visited, Some(&project_root))?,
            });
        }

        let rules_dir = project_root.join(RULES_DIR);
        for rule in sorted_markdown_files(&rules_dir)? {
            project_sections.push(InstructionSection {
                source: rule.clone(),
                content: read_with_includes(&rule, &mut visited, Some(&project_root))?,
            });
        }
    }
    apply_prompt_budget(&mut project_sections);
    sections.extend(project_sections);
    let trusted_workspace = cwd.canonicalize().ok().filter(|path| is_trusted(path));
    Ok(ProjectInstructions {
        sections,
        trusted_workspace,
    })
}

fn apply_prompt_budget(sections: &mut Vec<InstructionSection>) {
    let fits = sections
        .iter()
        .map(|section| section.source.to_string_lossy().len() + section.content.trim().len() + 6)
        .sum::<usize>()
        <= CHAIN_BYTES;
    if fits {
        return;
    }
    for section in sections.iter_mut() {
        section.content = section.content.trim().to_owned();
    }
    fit_chain(
        sections,
        |section| section.source.to_string_lossy().len() + 6,
        false,
    );
}

fn fit_chain(
    sections: &mut Vec<InstructionSection>,
    overhead: impl Fn(&InstructionSection) -> usize,
    record_digest: bool,
) {
    let mut remaining = sections
        .iter()
        .map(|section| overhead(section) + section.content.len())
        .sum::<usize>();
    if remaining <= CHAIN_BYTES {
        return;
    }
    let mut dropped = Vec::new();
    let mut skip = 0;
    while skip + 1 < sections.len() {
        let section = &sections[skip];
        remaining -= overhead(section) + section.content.len();
        dropped.push(section.source.display().to_string());
        skip += 1;
        let note = format!("{BUDGET_NOTE}Dropped {}.]", dropped.join(", "));
        if remaining + note.len() <= CHAIN_BYTES {
            sections.drain(..skip);
            sections.last_mut().unwrap().content.push_str(&note);
            return;
        }
    }
    sections.drain(..skip);
    let section = sections.last_mut().unwrap();
    let digest = if record_digest {
        format!(
            " Full text digest {}.",
            orca_core::hex::lower(&Sha256::digest(&section.content))
        )
    } else {
        String::new()
    };
    let mut note = format!("{BUDGET_NOTE}Cut {}.{digest}", section.source.display());
    if !dropped.is_empty() {
        note.push_str(&format!(" Dropped {}.", dropped.join(", ")));
    }
    let room = CHAIN_BYTES.saturating_sub(overhead(section) + 1);
    if note.len() > room {
        let end = note.floor_char_boundary(room.saturating_sub('…'.len_utf8()));
        note.truncate(end);
        note.push('…');
    }
    note.push(']');
    let limit = CHAIN_BYTES.saturating_sub(overhead(section) + note.len());
    section
        .content
        .truncate(section.content.floor_char_boundary(limit));
    section.content.push_str(&note);
}

fn nested_scope(source: &Path) -> String {
    source
        .parent()
        .expect("nested instruction directory")
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn nested_header(source: &Path, updated: bool) -> String {
    let dir = nested_scope(source);
    let prefix = if updated { UPDATED } else { ADDITIONAL };
    format!("{prefix}{dir}/ from {dir}/AGENTS.md:\n")
}

fn nested_parts(content: &str) -> Option<(&str, &str, bool)> {
    const SEPARATOR: &str = "/ from ";
    let (rest, updated) = if let Some(rest) = content.strip_prefix(ADDITIONAL) {
        (rest, false)
    } else {
        (content.strip_prefix(UPDATED)?, true)
    };
    for (end, _) in rest.match_indices(":\n") {
        let Some(repeated) = rest[..end].strip_suffix("/AGENTS.md") else {
            continue;
        };
        let middle = repeated.len().saturating_sub(SEPARATOR.len()) / 2;
        if let Some((left, right)) = repeated.split_at_checked(middle)
            && !left.is_empty()
            && right.strip_prefix(SEPARATOR) == Some(left)
        {
            return Some((left, &rest[end + 2..], updated));
        }
    }
    None
}

pub(crate) fn is_nested_message(content: &str) -> bool {
    nested_parts(content).is_some()
}

fn instruction_digest(body: &str) -> String {
    let Some((text, note)) = body
        .rsplit_once(BUDGET_NOTE)
        .filter(|(_, note)| note.ends_with(']'))
    else {
        return orca_core::hex::lower(&Sha256::digest(body));
    };
    if let Some((_, value)) = note.split_once(" Full text digest ")
        && let Some((hex, _)) = value.split_once('.')
        && hex.len() == 64
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return hex.to_owned();
    }
    orca_core::hex::lower(&Sha256::digest(text))
}

pub(crate) fn restore_nested_state(
    conversation: &mut Conversation,
    cwd: &Path,
    messages: &[Message],
) {
    use orca_core::tool_types::ToolStatus;
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut writes = HashMap::new();
    for message in messages {
        match message {
            Message::Assistant { tool_calls, .. } => {
                writes.clear();
                writes.extend(tool_calls.iter().filter_map(|call| {
                    matches!(call.function_name.as_str(), "write_file" | "edit")
                        .then_some((call.id.as_str(), call.function_name.as_str()))
                }));
            }
            Message::Tool {
                tool_call_id,
                content,
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Completed => {
                let path = match writes.remove(tool_call_id.as_str()) {
                    Some("edit") => content.strip_prefix("edited "),
                    Some("write_file") => content
                        .strip_prefix("wrote ")
                        .and_then(|text| text.split_once(" bytes to ").map(|(_, path)| path)),
                    _ => None,
                };
                if let Some(path) = path.map(|path| cwd.join(path))
                    && path
                        .file_name()
                        .is_some_and(|name| name == INSTRUCTIONS_FILE)
                    && let Ok(path) = path.canonicalize()
                {
                    conversation
                        .nested_instructions
                        .written_sources
                        .insert(path);
                }
            }
            Message::System { content, .. } => {
                if let Some((dir, body, updated)) = nested_parts(content) {
                    conversation
                        .nested_instructions
                        .versions
                        .insert(dir.to_owned(), (instruction_digest(body), updated));
                    conversation
                        .nested_instructions
                        .written_sources
                        .remove(&cwd.join(dir).join(INSTRUCTIONS_FILE));
                }
            }
            _ => {}
        }
    }
}

impl ProjectInstructions {
    pub(crate) fn append_nested_for_touches(
        &self,
        conversation: &mut Conversation,
        mut writer: Option<&mut SessionWriter>,
    ) {
        let touches = std::mem::take(&mut conversation.nested_instructions.pending_touches);
        let Some(root) = &self.trusted_workspace else {
            return;
        };
        let touches = touches
            .into_iter()
            .filter_map(|(path, modified)| {
                let path = path.canonicalize().ok()?;
                if !path.starts_with(root) {
                    return None;
                }
                let wrote_agents = modified
                    && path
                        .file_name()
                        .is_some_and(|name| name == INSTRUCTIONS_FILE);
                Some((path, wrote_agents))
            })
            .collect::<Vec<_>>();
        let last_writes = touches
            .iter()
            .enumerate()
            .filter_map(|(index, (path, wrote_agents))| wrote_agents.then_some((path, index)))
            .collect::<HashMap<_, _>>();
        if touches.is_empty() {
            return;
        }
        let mut visible = conversation
            .messages
            .iter()
            .filter_map(|message| {
                let Message::System { content, .. } = message else {
                    return None;
                };
                let (scope, body, _) = nested_parts(content)?;
                Some((scope.to_owned(), instruction_digest(body)))
            })
            .collect::<HashMap<_, _>>();
        for (index, (path, written_agents)) in touches.iter().enumerate() {
            let Some(parent) = path.parent() else {
                continue;
            };
            if !parent.starts_with(root) {
                continue;
            }
            if *written_agents && parent != root {
                conversation
                    .nested_instructions
                    .written_sources
                    .insert(path.clone());
            }
            let mut sections = parent
                .ancestors()
                .take_while(|dir| *dir != root)
                .filter_map(|dir| {
                    let source = dir.join(INSTRUCTIONS_FILE);
                    if last_writes
                        .get(&source)
                        .is_some_and(|write| *write >= index)
                        || source.canonicalize().ok().as_ref() != Some(&source)
                    {
                        return None;
                    }
                    let content = fs::read_to_string(&source).ok()?;
                    Some(InstructionSection {
                        source: source.strip_prefix(root).ok()?.to_owned(),
                        content,
                    })
                })
                .collect::<Vec<_>>();
            sections.reverse();
            fit_chain(
                &mut sections,
                |section| nested_header(&section.source, false).len(),
                true,
            );
            for section in sections {
                let dir = nested_scope(&section.source);
                let version = instruction_digest(&section.content);
                let was_written = conversation
                    .nested_instructions
                    .written_sources
                    .remove(&root.join(&section.source));
                let previous = conversation.nested_instructions.versions.get(&dir);
                let already_recorded = previous.is_some_and(|(seen, _)| *seen == version);
                if already_recorded && visible.get(&dir) == Some(&version) {
                    continue;
                }
                let updated = match previous {
                    Some((_, updated)) if already_recorded => *updated,
                    Some(_) => true,
                    None => was_written,
                };
                visible.insert(dir.clone(), version.clone());
                conversation
                    .nested_instructions
                    .versions
                    .insert(dir, (version, updated));
                let message = Message::system(format!(
                    "{}{}",
                    nested_header(&section.source, updated),
                    section.content
                ));
                // If compaction removed this version, make it available on this
                // touch using the existing transcript record.
                if !already_recorded
                    && let Some(writer) = writer.as_deref_mut()
                    && let Err(error) = writer.append_message(&message)
                {
                    tracing::warn!(%error, "nested instructions could not be saved to the session");
                    writer.mark_degraded();
                }
                conversation.messages.push(message);
            }
        }
    }
}

fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    // Search both the original path and canonical path to handle symlinks
    for start in [Some(cwd.to_path_buf()), cwd.canonicalize().ok()]
        .into_iter()
        .flatten()
    {
        for candidate in start.ancestors() {
            if candidate.join(".git").exists()
                || candidate.join("Cargo.toml").exists()
                || candidate.join("package.json").exists()
            {
                return Some(candidate.to_path_buf());
            }
        }
    }
    None
}

fn sorted_markdown_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_with_includes(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    allowed_root: Option<&Path>,
) -> io::Result<String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cyclic @include in {}", path.display()),
        ));
    }

    let content = fs::read_to_string(path)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut output = String::new();
    let mut seen_includes = HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(include_path) = trimmed.strip_prefix("@include ") {
            let include_path = include_path.trim();
            if include_path.is_empty() {
                continue;
            }
            let resolved = base_dir.join(include_path);
            // Path traversal guard: included file must reside within allowed_root
            if let Some(root) = allowed_root {
                let resolved_canonical =
                    resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
                let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
                if !resolved_canonical.starts_with(&root_canonical) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("@include path escapes project root: {}", include_path),
                    ));
                }
            }
            if seen_includes.insert(resolved.clone()) {
                let included = read_with_includes(&resolved, visited, allowed_root)?;
                output.push_str(included.trim());
                output.push('\n');
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn trust_project(home: &Path, project: &Path) {
        orca_core::config::folder_trust::set_trust_with_config_dir(
            project,
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
    }

    use crate::history::SessionWriter;
    use orca_core::conversation::{Conversation, Message};

    fn nested_project() -> (TempDir, TempDir, ProjectInstructions) {
        let project = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        fs::create_dir(project.path().join("src")).unwrap();
        fs::write(project.path().join("AGENTS.md"), "Root rules").unwrap();
        fs::write(project.path().join("src/one.txt"), "one").unwrap();
        fs::write(project.path().join("src/two.txt"), "two").unwrap();
        trust_project(home.path(), project.path());
        let instructions = load_for_cwd_with_home(project.path(), Some(home.path())).unwrap();
        (project, home, instructions)
    }

    fn touch(
        instructions: &ProjectInstructions,
        conversation: &mut Conversation,
        mut writer: Option<&mut SessionWriter>,
        project: &Path,
        path: &str,
    ) {
        use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
        let request = ToolRequest {
            id: path.into(),
            name: ToolName::ReadFile,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({"path": path}).to_string()),
        };
        let result = orca_tools::registry::default_tool_registry()
            .execute(&request, &orca_tools::ToolContext::new(project));
        assert_eq!(result.status, ToolStatus::Completed);
        crate::session::record_tool_result_for_agent(
            conversation,
            writer.as_deref_mut(),
            &result,
            false,
        )
        .unwrap();
        instructions.append_nested_for_touches(conversation, writer);
    }

    fn nested_messages(conversation: &Conversation) -> Vec<&str> {
        conversation
            .messages
            .iter()
            .filter_map(|msg| match msg {
                Message::System { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn touching_a_file_below_a_nested_agents_md_appends_it_once() {
        let (project, _home, instructions) = nested_project();
        fs::write(project.path().join("src/AGENTS.md"), "Nested rules").unwrap();
        let mut conversation = Conversation::new();
        for _ in 0..2 {
            touch(
                &instructions,
                &mut conversation,
                None,
                project.path(),
                "src/one.txt",
            );
        }
        assert_eq!(
            nested_messages(&conversation),
            ["Additional instructions for src/ from src/AGENTS.md:\nNested rules"]
        );
    }

    #[test]
    fn nested_instructions_are_appended_once_per_file_version() {
        let (project, _home, instructions) = nested_project();
        let mut conversation = Conversation::new();
        for body in ["First version", "Second version"] {
            fs::write(project.path().join("src/AGENTS.md"), body).unwrap();
            for path in ["src/one.txt", "src/two.txt"] {
                touch(&instructions, &mut conversation, None, project.path(), path);
            }
        }
        assert_eq!(nested_messages(&conversation).len(), 2);
    }

    #[test]
    fn an_updated_nested_agents_md_is_appended_as_updated() {
        let (project, _home, instructions) = nested_project();
        let mut conversation = Conversation::new();
        for body in ["Before", "After"] {
            fs::write(project.path().join("src/AGENTS.md"), body).unwrap();
            touch(
                &instructions,
                &mut conversation,
                None,
                project.path(),
                "src/one.txt",
            );
        }
        assert_eq!(
            nested_messages(&conversation)[1],
            "Updated instructions for src/ from src/AGENTS.md:\nAfter"
        );
    }

    #[test]
    fn an_agents_md_the_model_wrote_is_appended_once() {
        let (project, _home, instructions) = nested_project();
        let request = orca_core::tool_types::ToolRequest {
            id: "write-agents".into(),
            name: orca_core::tool_types::ToolName::WriteFile,
            action: orca_core::approval_types::ActionKind::Write,
            target: None,
            raw_arguments: Some(
                serde_json::json!({"path":"src/AGENTS.md","content":"Written rule"}).to_string(),
            ),
        };
        let result = orca_tools::registry::default_tool_registry()
            .execute(&request, &orca_tools::ToolContext::new(project.path()));
        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
        let mut conversation = Conversation::new();
        crate::session::record_tool_result_for_agent(&mut conversation, None, &result, false)
            .unwrap();
        instructions.append_nested_for_touches(&mut conversation, None);
        assert!(nested_messages(&conversation).is_empty());
        for path in ["src/one.txt", "src/AGENTS.md", "src/two.txt"] {
            touch(&instructions, &mut conversation, None, project.path(), path);
        }
        assert_eq!(
            nested_messages(&conversation),
            ["Updated instructions for src/ from src/AGENTS.md:\nWritten rule"]
        );
    }

    #[test]
    fn each_instruction_chain_has_its_own_budget() {
        let (project, home, _) = nested_project();
        fs::write(home.path().join("AGENTS.md"), "User instructions").unwrap();
        fs::write(project.path().join("AGENTS.md"), "é".repeat(50_000)).unwrap();
        let instructions = load_for_cwd_with_home(project.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();
        assert!(block.contains("User instructions"));
        assert!(block.len() < 66_000, "{} bytes", block.len());
        assert!(block.contains("Cut") && block.contains("AGENTS.md"));
        let child = project.path().join("src/inner");
        fs::create_dir(&child).unwrap();
        fs::write(project.path().join("src/AGENTS.md"), "a".repeat(60_000)).unwrap();
        fs::write(child.join("AGENTS.md"), "b".repeat(10_000)).unwrap();
        fs::write(child.join("one.txt"), "one").unwrap();
        let mut conversation = Conversation::new();
        touch(
            &instructions,
            &mut conversation,
            None,
            project.path(),
            "src/inner/one.txt",
        );
        let messages = nested_messages(&conversation).join("\n");
        assert!(!messages.contains(&"a".repeat(80)));
        assert!(messages.contains(&"b".repeat(10_000)) && messages.contains("src/AGENTS.md"));
    }

    #[test]
    fn the_root_chain_is_unchanged_when_it_fits() {
        let (project, home, _) = nested_project();
        fs::create_dir_all(project.path().join(RULES_DIR)).unwrap();
        fs::write(
            project.path().join(RULES_DIR).join("build.md"),
            "  Build rules  ",
        )
        .unwrap();
        let block = load_for_cwd_with_home(project.path(), Some(home.path()))
            .unwrap()
            .to_system_prompt_block()
            .unwrap();
        assert_eq!(
            block,
            format!(
                "<project-instructions>\n# {}/AGENTS.md\nRoot rules\n\n# {}/.orca/rules/build.md\nBuild rules\n</project-instructions>",
                project.path().display(),
                project.path().display()
            )
        );
    }

    #[test]
    fn nested_instructions_survive_resume_without_repeating() {
        let (project, _home, instructions) = nested_project();
        fs::write(project.path().join("src/AGENTS.md"), "Keep this rule").unwrap();
        crate::history::with_redirected_orca_home("nested-instructions", |_| {
            let mut writer =
                SessionWriter::start(project.path(), "mock", None, "instructions").unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            let mut conversation = Conversation::new();
            touch(
                &instructions,
                &mut conversation,
                Some(&mut writer),
                project.path(),
                "src/one.txt",
            );
            let id = writer.session_id().unwrap();
            let path = writer.path().to_path_buf();
            drop(writer);
            let mut resumed = crate::history::resume_conversation(
                &crate::history::load_session(&id).unwrap(),
                "base".into(),
            );
            resumed.messages.remove(0);
            assert_eq!(nested_messages(&resumed).len(), 1);
            let mut writer = SessionWriter::append_to_existing(path.clone()).unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            touch(
                &instructions,
                &mut resumed,
                Some(&mut writer),
                project.path(),
                "src/two.txt",
            );
            assert_eq!(nested_messages(&resumed).len(), 1);
            assert_eq!(crate::history::load_session(&id).unwrap().messages.len(), 1);
        });
    }

    #[test]
    fn nested_instructions_accept_a_directory_containing_the_header_separator() {
        let (project, _home, instructions) = nested_project();
        let directory = project.path().join("src/ from generated");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("AGENTS.md"), "Generated rules").unwrap();
        fs::write(directory.join("file.txt"), "sample").unwrap();
        let mut conversation = Conversation::new();
        touch(
            &instructions,
            &mut conversation,
            None,
            project.path(),
            "src/ from generated/file.txt",
        );
        assert_eq!(nested_messages(&conversation).len(), 1);
        touch(
            &instructions,
            &mut conversation,
            None,
            project.path(),
            "src/ from generated/file.txt",
        );
        assert_eq!(nested_messages(&conversation).len(), 1);
    }

    #[test]
    fn nested_instructions_remain_after_compaction_and_resume() {
        let (project, _home, instructions) = nested_project();
        fs::write(project.path().join("src/AGENTS.md"), "Keep this rule").unwrap();
        crate::history::with_redirected_orca_home("nested-compaction", |_| {
            let mut writer =
                SessionWriter::start(project.path(), "mock", None, "instructions").unwrap();
            let id = writer.session_id().unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            let mut conversation = Conversation::new();
            writer
                .append_message(&Message::system("base".into()))
                .unwrap();
            writer
                .append_message(&Message::user("old prompt".into()))
                .unwrap();
            touch(
                &instructions,
                &mut conversation,
                Some(&mut writer),
                project.path(),
                "src/one.txt",
            );
            writer.append_compaction(3, 2).unwrap();
            drop(writer);
            let mut resumed = crate::history::resume_conversation(
                &crate::history::load_session(&id).unwrap(),
                "base".into(),
            );
            touch(
                &instructions,
                &mut resumed,
                None,
                project.path(),
                "src/two.txt",
            );
            assert_eq!(
                nested_messages(&resumed),
                [
                    "base",
                    "Additional instructions for src/ from src/AGENTS.md:\nKeep this rule"
                ]
            );
        });
    }

    #[test]
    fn nested_instructions_remember_a_model_write_before_the_next_touch() {
        let (project, _home, instructions) = nested_project();
        crate::history::with_redirected_orca_home("nested-write-resume", |_| {
            use orca_core::{
                approval_types::ActionKind,
                conversation::RawToolCall,
                tool_types::{ToolName, ToolRequest},
            };
            let mut writer =
                SessionWriter::start(project.path(), "mock", None, "instructions").unwrap();
            let id = writer.session_id().unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            let arguments =
                serde_json::json!({"path":"src/AGENTS.md","content":"Written rule"}).to_string();
            let request = ToolRequest {
                id: "write-agents".into(),
                name: ToolName::WriteFile,
                action: ActionKind::Write,
                target: None,
                raw_arguments: Some(arguments.clone()),
            };
            writer
                .append_message(&Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: request.id.clone(),
                        function_name: "write_file".into(),
                        arguments,
                    }],
                    pinned: false,
                })
                .unwrap();
            let result = orca_tools::registry::default_tool_registry()
                .execute(&request, &orca_tools::ToolContext::new(project.path()));
            let mut conversation = Conversation::new();
            crate::session::record_tool_result_for_agent(
                &mut conversation,
                Some(&mut writer),
                &result,
                true,
            )
            .unwrap();
            instructions.append_nested_for_touches(&mut conversation, Some(&mut writer));
            assert!(nested_messages(&conversation).is_empty());
            drop(writer);
            let mut resumed = crate::history::resume_conversation(
                &crate::history::load_session(&id).unwrap(),
                "base".into(),
            );
            touch(
                &instructions,
                &mut resumed,
                None,
                project.path(),
                "src/one.txt",
            );
            assert_eq!(
                nested_messages(&resumed),
                [
                    "base",
                    "Updated instructions for src/ from src/AGENTS.md:\nWritten rule"
                ]
            );
        });
    }

    #[test]
    fn loads_agents_file_and_sorted_project_rules() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(dir.path().join(INSTRUCTIONS_FILE), "Root instructions\n").unwrap();
        fs::create_dir_all(dir.path().join(RULES_DIR)).unwrap();
        fs::write(
            dir.path().join(".orca/rules/020-second.md"),
            "Second rule\n",
        )
        .unwrap();
        fs::write(dir.path().join(".orca/rules/010-first.md"), "First rule\n").unwrap();
        trust_project(home.path(), dir.path());

        let instructions = load_for_cwd_with_home(dir.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();

        assert!(block.contains("<project-instructions>"));
        let root_index = block.find("Root instructions").unwrap();
        let first_index = block.find("First rule").unwrap();
        let second_index = block.find("Second rule").unwrap();
        assert!(root_index < first_index);
        assert!(first_index < second_index);
    }

    #[test]
    fn expands_relative_includes() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(dir.path().join("shared.md"), "Shared instruction\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "Before\n@include ./shared.md\nAfter\n",
        )
        .unwrap();
        trust_project(home.path(), dir.path());

        let instructions = load_for_cwd_with_home(dir.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();

        assert!(block.contains("Before\nShared instruction\nAfter"));
    }

    #[test]
    fn include_rejects_path_traversal_outside_project_root() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "@include ../../etc/passwd\n",
        )
        .unwrap();
        trust_project(home.path(), dir.path());

        let result = load_for_cwd_with_home(dir.path(), Some(home.path()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("escapes project root"));
    }

    #[test]
    fn untrusted_project_instructions_are_not_loaded() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(home.path().join(INSTRUCTIONS_FILE), "User instructions\n").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "Untrusted project instructions\n",
        )
        .unwrap();

        let instructions = load_for_cwd_with_home(dir.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();

        assert!(block.contains("User instructions"));
        assert!(!block.contains("Untrusted project instructions"));
    }
}
