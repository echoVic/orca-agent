use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use orca_core::conversation::{
    Conversation, InternalContextKind, InternalContextOrigin, Message, NestedInstructionSource,
    NestedInstructionVersion,
};
use sha2::{Digest, Sha256};

use crate::history::SessionWriter;

const INSTRUCTIONS_FILE: &str = "AGENTS.md";
const RULES_DIR: &str = ".orca/rules";
const CHAIN_BYTES: usize = 65_536;
const BUDGET_NOTE: &str = "\n\n[Instruction budget: ";
const ADDITIONAL: &str = "Additional instructions for ";
const UPDATED: &str = "Updated instructions for ";
const NESTED_CONTEXT: &str = "nested-project-instructions";
const PROJECT_OPEN: &str = "<project-instructions>\n";
const PROJECT_CLOSE: &str = "</project-instructions>";

#[derive(Clone, Debug, Default)]
pub struct ProjectInstructions {
    sections: Vec<InstructionSection>,
    trusted_workspace: Option<PathBuf>,
    trust_home: Option<PathBuf>,
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

        let mut block = String::from(PROJECT_OPEN);
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
        block.push_str(PROJECT_CLOSE);
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
        trust_home: orca_home_path.map(Path::to_owned),
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
    fit_root_chain(sections, |section| {
        section.source.to_string_lossy().len() + 6
    });
}

fn fit_root_chain(
    sections: &mut Vec<InstructionSection>,
    overhead: impl Fn(&InstructionSection) -> usize,
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
    let mut note = format!("{BUDGET_NOTE}Cut {}.", section.source.display());
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

fn display_source(source: &Path) -> String {
    source
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_path(path: &str) -> String {
    escape_text(path)
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
}

fn nested_header(identity: &NestedInstructionSource) -> String {
    let source = escape_path(&display_source(&identity.source));
    let scope = source.strip_suffix(INSTRUCTIONS_FILE).unwrap_or(&source);
    let prefix = if identity.updated {
        UPDATED
    } else {
        ADDITIONAL
    };
    format!("# {prefix}{scope} from {source}:\n")
}

pub(crate) fn render_nested_record(identity: &NestedInstructionSource, content: &str) -> String {
    format!(
        "{PROJECT_OPEN}{}{}\n{PROJECT_CLOSE}",
        nested_header(identity),
        escape_text(content)
    )
}

fn trusted_source(workspace: &Path, source: &Path, home: &Path) -> bool {
    use std::path::Component;
    source
        .file_name()
        .is_some_and(|name| name == INSTRUCTIONS_FILE)
        && source
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && source.parent().is_some_and(|parent| {
            !parent.as_os_str().is_empty()
                && orca_core::config::folder_trust::is_trusted_project_scope(
                    workspace,
                    &workspace.join(parent),
                    home,
                )
        })
}

fn render_effective_chain(conversation: &mut Conversation, workspace: &Path, home: Option<&Path>) {
    let mut versions = conversation
        .nested_instructions
        .versions
        .values()
        .filter(|version| {
            version.identity.workspace == workspace
                && home
                    .is_some_and(|home| trusted_source(workspace, &version.identity.source, home))
        })
        .collect::<Vec<_>>();
    versions.sort_by_key(|version| {
        (
            version.identity.source.components().count(),
            &version.identity.source,
        )
    });
    let block = if versions.is_empty() {
        None
    } else {
        Some(fit_nested_chain(&versions))
    };
    conversation.replace_internal_context(
        NESTED_CONTEXT,
        InternalContextKind::ProjectInstructions,
        InternalContextOrigin::System,
        block,
        CHAIN_BYTES,
    );
}

fn fit_nested_chain(versions: &[&NestedInstructionVersion]) -> String {
    let sections = versions
        .iter()
        .map(|version| {
            let mut rendered = nested_header(&version.identity);
            rendered.push_str(&escape_text(&version.content));
            rendered.push('\n');
            rendered
        })
        .collect::<Vec<_>>();
    let room = CHAIN_BYTES - PROJECT_OPEN.len() - PROJECT_CLOSE.len();
    let mut remaining = sections.iter().map(String::len).sum::<usize>();
    let mut dropped = Vec::new();
    for (index, (version, section)) in versions.iter().zip(&sections).enumerate() {
        let source = escape_path(&display_source(&version.identity.source));
        let dropped_note = if dropped.is_empty() {
            String::new()
        } else {
            format!(" Dropped {}.", dropped.join(", "))
        };
        let note = if dropped.is_empty() {
            String::new()
        } else {
            format!("{BUDGET_NOTE}{}]\n", dropped_note.trim_start())
        };
        if remaining + note.len() <= room
            && !versions[index..]
                .iter()
                .any(|version| version.identity.shortened)
        {
            return format!(
                "{PROJECT_OPEN}{}{note}{PROJECT_CLOSE}",
                sections[index..].concat()
            );
        }
        if index + 1 < sections.len() {
            remaining -= section.len();
            dropped.push(source);
            continue;
        }
        let mut note = format!("{BUDGET_NOTE}Cut {source}.{dropped_note}]\n");
        let header = nested_header(&version.identity);
        let note_room = room.saturating_sub(header.len());
        if note_room < "…]\n".len() {
            return format!(
                "{PROJECT_OPEN}[Instruction budget: Dropped nested instructions because the source path exceeds the budget.]\n{PROJECT_CLOSE}"
            );
        }
        if note.len() > note_room {
            note.truncate(note.floor_char_boundary(note_room.saturating_sub("…]\n".len())));
            note.push_str("…]\n");
        }
        let body = &section[header.len()..section.len() - 1];
        let body_room = room.saturating_sub(header.len() + note.len());
        let mut end = body.floor_char_boundary(body_room);
        if let Some((start, _)) = body[..end].rmatch_indices(['&', ';']).next()
            && body.as_bytes()[start] == b'&'
        {
            end = start;
        }
        return format!(
            "{PROJECT_OPEN}{header}{}{note}{PROJECT_CLOSE}",
            &body[..end]
        );
    }
    unreachable!("a nested chain has a source")
}

fn read_nested_source(path: &Path) -> io::Result<(String, String, bool)> {
    let mut file = orca_platform::fs::open_confined_regular(path)?;
    let mut digest = Sha256::new();
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8192 + 3];
    let mut pending = 0;
    let mut shortened = false;
    loop {
        let read = match file.read(&mut buffer[pending..]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        digest.update(&buffer[pending..pending + read]);
        let bytes = &buffer[..pending + read];
        let valid = match std::str::from_utf8(bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        if !shortened {
            let text = std::str::from_utf8(&bytes[..valid]).expect("checked UTF-8 prefix");
            let len = text.floor_char_boundary(CHAIN_BYTES - retained.len());
            retained.extend_from_slice(&bytes[..len]);
            shortened = len < valid;
        }
        pending = bytes.len() - valid;
        buffer.copy_within(valid..valid + pending, 0);
    }
    if pending != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete UTF-8 in project instructions",
        ));
    }
    Ok((
        String::from_utf8(retained).expect("checked UTF-8 prefix"),
        orca_core::hex::lower(&digest.finalize()),
        shortened,
    ))
}

pub(crate) fn restore_nested_state(
    conversation: &mut Conversation,
    cwd: &Path,
    messages: &[Message],
    versions: &[NestedInstructionVersion],
    written_sources: &[PathBuf],
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
            _ => {}
        }
    }
    conversation
        .nested_instructions
        .written_sources
        .extend(written_sources.iter().cloned());
    for version in versions
        .iter()
        .filter(|version| version.identity.workspace == cwd)
    {
        conversation
            .nested_instructions
            .written_sources
            .remove(&cwd.join(&version.identity.source));
        conversation
            .nested_instructions
            .versions
            .insert(version.identity.source.clone(), version.clone());
    }
    render_effective_chain(conversation, &cwd, orca_core::home::orca_home().as_deref());
}

pub(crate) fn refresh_nested_instructions(conversation: &mut Conversation, cwd: &Path) {
    if conversation.nested_instructions.versions.is_empty() {
        return;
    }
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    render_effective_chain(conversation, &cwd, orca_core::home::orca_home().as_deref());
}

impl ProjectInstructions {
    pub(crate) fn append_nested_for_touches(
        &self,
        conversation: &mut Conversation,
        mut writer: Option<&mut SessionWriter>,
    ) {
        let touches = std::mem::take(&mut conversation.nested_instructions.pending_touches);
        let Some((root, home)) = self
            .trusted_workspace
            .as_ref()
            .zip(self.trust_home.as_deref())
        else {
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
        for (index, (path, written_agents)) in touches.iter().enumerate() {
            let Some(parent) = path.parent().filter(|parent| parent.starts_with(root)) else {
                continue;
            };
            if *written_agents && parent != root {
                conversation
                    .nested_instructions
                    .written_sources
                    .insert(path.clone());
            }
            for dir in parent.ancestors().take_while(|dir| *dir != root) {
                let path = dir.join(INSTRUCTIONS_FILE);
                let source = path.strip_prefix(root).expect("workspace ancestor");
                if last_writes.get(&path).is_some_and(|write| *write >= index)
                    || !trusted_source(root, source, home)
                {
                    continue;
                }
                let Ok((content, digest, shortened)) = read_nested_source(&path) else {
                    continue;
                };
                let state = &mut conversation.nested_instructions;
                let written = state.written_sources.remove(&path);
                let previous = state.versions.get(source);
                if previous.is_some_and(|previous| previous.identity.digest == digest) {
                    continue;
                }
                let version = NestedInstructionVersion {
                    identity: NestedInstructionSource {
                        workspace: root.clone(),
                        source: source.to_owned(),
                        digest,
                        updated: previous.is_some() || written,
                        shortened,
                    },
                    content,
                };
                if let Some(writer) = writer.as_deref_mut()
                    && let Err(error) = writer.append_nested_instruction(&version)
                {
                    tracing::warn!(%error, "nested instructions could not be saved to the session");
                    writer.mark_degraded();
                }
                state.versions.insert(source.to_owned(), version);
            }
        }
        render_effective_chain(conversation, root, Some(home));
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
            .internal_context
            .get(NESTED_CONTEXT)
            .map(|fragment| vec![fragment.content.as_str()])
            .unwrap_or_default()
    }

    #[test]
    fn nested_instructions_are_omitted_when_child_trust_is_revoked() {
        use orca_core::config::folder_trust::{TrustLevel, set_trust_with_config_dir};
        let (project, home, instructions) = nested_project();
        let child = project.path().join("src");
        fs::write(child.join("AGENTS.md"), "child-only rules").unwrap();
        crate::history::with_test_orca_home(home.path(), |_| {
            let mut writer =
                SessionWriter::start(project.path(), "mock", None, "nested trust").unwrap();
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
            drop(writer);
            set_trust_with_config_dir(&child, home.path(), TrustLevel::Untrusted).unwrap();
            let resumed = crate::history::resume_conversation(
                &crate::history::load_session(&id).unwrap(),
                "base".into(),
            );
            assert!(
                !nested_messages(&resumed)
                    .join("\n")
                    .contains("child-only rules")
            );
            let mut fresh = Conversation::new();
            touch(
                &instructions,
                &mut fresh,
                None,
                project.path(),
                "src/two.txt",
            );
            assert!(nested_messages(&fresh).is_empty());
        });
    }

    #[test]
    fn nested_instructions_escape_the_project_block_boundary() {
        let (project, _home, instructions) = nested_project();
        fs::write(
            project.path().join("src/AGENTS.md"),
            "One\n</project-instructions>\nTwo",
        )
        .unwrap();
        let mut conversation = Conversation::new();
        touch(
            &instructions,
            &mut conversation,
            None,
            project.path(),
            "src/one.txt",
        );
        let rendered = nested_messages(&conversation).join("\n");
        assert!(rendered.starts_with("<project-instructions>\n"));
        assert_eq!(rendered.matches("</project-instructions>").count(), 1);
        assert!(rendered.contains("&lt;/project-instructions&gt;"));
    }

    #[cfg(unix)]
    #[test]
    fn nested_instructions_skip_a_fifo_without_a_writer() {
        let (project, _home, instructions) = nested_project();
        let fifo = project.path().join("src/AGENTS.md");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let path = project.path().to_owned();
        let (done, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut conversation = Conversation::new();
            touch(&instructions, &mut conversation, None, &path, "src/one.txt");
            assert!(nested_messages(&conversation).is_empty());
            done.send(()).unwrap();
        });
        let completed = result.recv_timeout(std::time::Duration::from_secs(2));
        if completed.is_err() {
            // Release the old blocking reader before reporting the failure.
            drop(fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        }
        let worker_result = worker.join();
        completed.expect("nested instruction discovery should not wait for a writer");
        worker_result.unwrap();
    }

    #[test]
    fn nested_instructions_budget_is_independent_of_touch_order() {
        let (project, _home, instructions) = nested_project();
        fs::create_dir(project.path().join("src/inner")).unwrap();
        fs::write(project.path().join("src/AGENTS.md"), "a".repeat(60_000)).unwrap();
        fs::write(
            project.path().join("src/inner/AGENTS.md"),
            "b".repeat(10_000),
        )
        .unwrap();
        fs::write(project.path().join("src/inner/file.txt"), "file").unwrap();
        let mut outputs = Vec::new();
        for paths in [
            ["src/one.txt", "src/inner/file.txt"],
            ["src/inner/file.txt", "src/one.txt"],
        ] {
            let mut conversation = Conversation::new();
            for path in paths {
                touch(&instructions, &mut conversation, None, project.path(), path);
            }
            outputs.push(nested_messages(&conversation).join("\n"));
        }
        assert!(
            outputs
                .iter()
                .all(|output| !output.contains(&"a".repeat(80)))
        );
        assert_eq!(outputs[0], outputs[1]);
    }

    #[test]
    fn nested_instructions_dedup_across_redaction_and_a_manual_snapshot() {
        use orca_core::config::{HistoryMode, ModelRuntimeConfig, ProviderKind, RunConfig};
        let (project, home, instructions) = nested_project();
        let cwd = project.path();
        let rule = "Keep this rule. api_key=sample-value";
        fs::write(cwd.join("src/AGENTS.md"), rule).unwrap();
        crate::history::with_test_orca_home(home.path(), |_| {
            let config = RunConfig {
                cwd: Some(cwd.to_owned()),
                history_mode: HistoryMode::Record,
                provider: ProviderKind::Mock,
                model_runtime: ModelRuntimeConfig {
                    soft_compact_token_limit: Some(4_000),
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut session = crate::session::InteractiveSession::new_with_preloaded(
                &config,
                "nested snapshot",
                None,
            )
            .unwrap();
            let mut conversation =
                std::mem::replace(session.conversation_mut(), Conversation::new());
            let writer = session.writer_mut().unwrap();
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            let old = "old history marker ".repeat(3_500);
            writer.append_message(&Message::user(old.clone())).unwrap();
            conversation.add_user(old);
            touch(
                &instructions,
                &mut conversation,
                Some(writer),
                cwd,
                "src/one.txt",
            );
            let id = writer.session_id().unwrap();
            let transcript = crate::history::load_session(&id).unwrap();
            assert_eq!(
                transcript.nested_instructions[0].identity.digest,
                orca_core::hex::lower(&Sha256::digest(rule))
            );
            assert_eq!(
                transcript.nested_instructions[0].identity.workspace,
                cwd.canonicalize().unwrap()
            );
            let mut ordinary = crate::history::resume_conversation(&transcript, "base".into());
            let before_touch = nested_messages(&ordinary).join("\n");
            touch(
                &instructions,
                &mut ordinary,
                Some(writer),
                cwd,
                "src/two.txt",
            );
            assert_eq!(nested_messages(&ordinary), [before_touch.as_str()]);
            assert_eq!(
                crate::history::load_session(&id).unwrap().messages.len(),
                transcript.messages.len()
            );
            writer
                .append_message(&Message::user("retained tail".into()))
                .unwrap();
            conversation.add_user("retained tail".into());
            *session.conversation_mut() = conversation;
            let identity = crate::session::ManualCompactionPersistenceIdentity {
                operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .unwrap(),
                snapshot_id: uuid::Uuid::now_v7().to_string(),
            };
            let operation_id = identity.operation_id.clone();
            let outcome = session
                .compact(&config, cwd, &orca_core::cancel::CancelToken::new(), |_| {
                    Ok(identity)
                })
                .unwrap();
            assert!(outcome.before.messages.iter().any(|msg| {
                msg.content_str()
                    .is_some_and(|text| text.starts_with("old history marker"))
            }));
            assert!(!outcome.after.messages.iter().any(|msg| {
                msg.content_str()
                    .is_some_and(|text| text.starts_with("old history marker"))
            }));
            let snapshot = session
                .manual_compaction_snapshot(&operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(snapshot.conversation.nested_instructions.versions.len(), 1);
            let mut resumed = crate::history::resume_conversation(
                &crate::history::load_session(&id).unwrap(),
                "base".into(),
            );
            let before = nested_messages(&resumed).join("\n");
            let count = crate::history::load_session(&id).unwrap().messages.len();
            touch(
                &instructions,
                &mut resumed,
                session.writer_mut(),
                cwd,
                "src/two.txt",
            );
            assert!(before.contains("Keep this rule. api_key=&lt;redacted&gt;"));
            assert_eq!(nested_messages(&resumed), [before.as_str()]);
            assert_eq!(
                crate::history::load_session(&id).unwrap().messages.len(),
                count
            );
        });
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
            [
                "<project-instructions>\n# Additional instructions for src/ from src/AGENTS.md:\nNested rules\n</project-instructions>"
            ]
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
        let messages = nested_messages(&conversation).join("\n");
        assert!(!messages.contains("First version"));
        assert_eq!(messages.matches("Second version").count(), 1);
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
            nested_messages(&conversation)[0],
            "<project-instructions>\n# Updated instructions for src/ from src/AGENTS.md:\nAfter\n</project-instructions>"
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
            [
                "<project-instructions>\n# Updated instructions for src/ from src/AGENTS.md:\nWritten rule\n</project-instructions>"
            ]
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
    fn root_instructions_preserve_the_full_provider_request() {
        fn request_body(conversation: &Conversation) -> Vec<u8> {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::net::TcpListener;
            use std::time::{Duration, Instant};
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let stop = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < stop =>
                        {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("provider fixture: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut length = None;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.unwrap()];
                reader.read_exact(&mut body).unwrap();
                let reply = r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
                body
            });
            let config = orca_provider::ProviderConfig {
                api_key: Some("local-fixture".into()),
                base_url: Some(url),
                model: Some("deepseek-flash".into()),
                tools_override: Some(vec![]),
                reasoning_effort: Default::default(),
                mcp_registry: None,
                external_tools: vec![],
                max_output_tokens: Some(1024),
            };
            let result = orca_provider::deepseek_http::call(conversation, &config);
            assert_eq!(result.assistant_content.as_deref(), Some("ok"));
            server.join().unwrap()
        }

        let (project, _home, instructions) = nested_project();
        let startup = |instructions| {
            crate::agent_common::build_agent_system_prompt(
                project.path(),
                0,
                &orca_core::subagent_types::SubagentType::General,
                instructions,
                orca_core::approval_types::ApprovalMode::Suggest,
                None,
            )
        };
        let base = startup(None);
        let plain = crate::system_prompt::build_system_prompt_for_tools(project.path(), None);
        assert!(base.starts_with(&plain));
        let boundary = plain.len();
        let expected = format!(
            "{}\n\n<project-instructions>\n# {}\nRoot rules\n</project-instructions>{}",
            &base[..boundary],
            project.path().join("AGENTS.md").display(),
            &base[boundary..]
        );
        let conversation = |system| {
            let mut conversation = Conversation::new();
            conversation.add_system(system);
            conversation.add_user("Check the project".into());
            conversation
        };
        assert_eq!(
            request_body(&conversation(startup(Some(&instructions)))),
            request_body(&conversation(expected))
        );
    }

    #[test]
    fn nested_instructions_survive_resume_without_repeating() {
        let (project, home, instructions) = nested_project();
        fs::write(project.path().join("src/AGENTS.md"), "Keep this rule").unwrap();
        crate::history::with_test_orca_home(home.path(), |_| {
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
        let (project, home, instructions) = nested_project();
        fs::write(project.path().join("src/AGENTS.md"), "Keep this rule").unwrap();
        crate::history::with_test_orca_home(home.path(), |_| {
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
                    "<project-instructions>\n# Additional instructions for src/ from src/AGENTS.md:\nKeep this rule\n</project-instructions>"
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
                    "<project-instructions>\n# Updated instructions for src/ from src/AGENTS.md:\nWritten rule\n</project-instructions>"
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
