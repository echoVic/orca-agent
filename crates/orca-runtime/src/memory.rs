use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::conversation::{Conversation, Message};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model;
use orca_core::provider_types::ProviderStep;
use orca_platform::PlatformError;
use orca_platform::fs::ExclusiveFileLock;
use orca_provider::{self, ProviderConfig};

const MEMORY_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default)]
pub struct MemoryBlock {
    pub user: Option<String>,
    pub project: Option<String>,
}

impl MemoryBlock {
    pub fn is_empty(&self) -> bool {
        self.user
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
            && self
                .project
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
    }

    pub fn to_system_prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut block = String::from("<memory>\n");
        if let Some(user) = self.user.as_deref().filter(|text| !text.trim().is_empty()) {
            block.push_str("<user>\n");
            block.push_str(user.trim());
            block.push_str("\n</user>\n");
        }
        if let Some(project) = self
            .project
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            block.push_str("<project>\n");
            block.push_str(project.trim());
            block.push_str("\n</project>\n");
        }
        block.push_str("</memory>");
        Some(block)
    }
}

pub fn load_for_cwd(cwd: &Path) -> MemoryBlock {
    let Some(root) = memory_root() else {
        return MemoryBlock::default();
    };
    MemoryBlock {
        user: read_trimmed(root.join("user.md")),
        project: read_trimmed(project_memory_path(&root, cwd)),
    }
}

pub fn remember_user(note: &str) -> Result<PathBuf, String> {
    let Some(root) = memory_root() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    let path = root.join("user.md");
    append_note(&path, note)?;
    Ok(path)
}

pub fn remember_project(cwd: &Path, note: &str) -> Result<PathBuf, String> {
    let Some(root) = memory_root() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    let path = project_memory_path(&root, cwd);
    append_note(&path, note)?;
    Ok(path)
}

pub fn extract_project_memory(
    provider_kind: ProviderKind,
    provider_config: &ProviderConfig,
    cwd: &Path,
    messages: &[Message],
) -> Result<Option<PathBuf>, String> {
    extract_project_memory_with_cancel(
        provider_kind,
        provider_config,
        cwd,
        messages,
        &CancelToken::new(),
    )
}

pub fn extract_project_memory_with_cancel(
    provider_kind: ProviderKind,
    provider_config: &ProviderConfig,
    cwd: &Path,
    messages: &[Message],
    cancel: &CancelToken,
) -> Result<Option<PathBuf>, String> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let source = format_messages_for_memory(messages);
    if source.trim().is_empty() {
        return Ok(None);
    }

    let mut conversation = Conversation::new();
    conversation.add_system(
        "Extract durable project memory from this coding session. Return only concise bullet points worth remembering for future sessions. If nothing is worth remembering, return NOTHING.".to_string(),
    );
    conversation.add_user(source);
    let summary_config = ProviderConfig {
        api_key: provider_config.api_key.clone(),
        base_url: provider_config.base_url.clone(),
        model: provider_config
            .model
            .clone()
            .or_else(|| Some("deepseek-v4-flash".to_string())),
        reasoning_effort: provider_config.reasoning_effort,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let response = orca_provider::call_streaming(
        provider_kind,
        &conversation,
        &summary_config,
        cancel,
        &mut |_| {},
    );
    if cancel.is_cancelled() {
        return Ok(None);
    }
    if response
        .steps
        .iter()
        .any(|step| matches!(step, ProviderStep::Error(_)))
    {
        return Ok(None);
    }
    let Some(note) = response
        .assistant_content
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "NOTHING")
    else {
        return Ok(None);
    };
    let Some(root) = memory_root() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    let path = project_memory_path(&root, cwd);
    append_note_with_cancel(&path, &note, cancel).map(|written| written.then_some(path))
}

pub(crate) fn extract_project_memory_after_final_response(
    config: &RunConfig,
    cwd: &Path,
    messages: &[Message],
    cancel: &CancelToken,
    events: &mut EventFactory,
    sink: &mut EventSink<impl std::io::Write>,
) -> Result<(), std::io::Error> {
    let provider_config = auto_memory_provider_config(config);
    if let Err(error) =
        extract_project_memory_with_cancel(config.provider, &provider_config, cwd, messages, cancel)
    {
        sink.emit(events.error(&format!("memory extraction failed: {error}")))?;
    }
    Ok(())
}

fn auto_memory_provider_config(config: &RunConfig) -> ProviderConfig {
    ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(model::auxiliary_model().to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    }
}

fn memory_root() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|root| root.join("memory"))
}

fn project_memory_path(root: &Path, cwd: &Path) -> PathBuf {
    root.join("projects")
        .join(format!("{:016x}", project_hash(cwd)))
        .join("memory.md")
}

fn project_hash(cwd: &Path) -> u64 {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let bytes = canonical.display().to_string();
    fnv1a_hash(bytes.as_bytes())
}

fn fnv1a_hash(data: &[u8]) -> u64 {
    const BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn append_note(path: &Path, note: &str) -> Result<(), String> {
    append_note_with_cancel(path, note, &CancelToken::new()).map(|_| ())
}

fn append_note_with_cancel(path: &Path, note: &str, cancel: &CancelToken) -> Result<bool, String> {
    let note = note.trim();
    if note.is_empty() {
        return Err("memory note cannot be empty".to_string());
    }
    let Some(_lock) = acquire_memory_lock(path, cancel)? else {
        return Ok(false);
    };
    if cancel.is_cancelled() {
        return Ok(false);
    }
    let created = !path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("failed to open memory file: {error}"))?;
    if cancel.is_cancelled() {
        if created
            && file
                .metadata()
                .map(|metadata| metadata.len() == 0)
                .unwrap_or(false)
        {
            drop(file);
            let _ = fs::remove_file(path);
        }
        return Ok(false);
    }
    writeln!(file, "- {note}").map_err(|error| format!("failed to write memory: {error}"))?;
    file.flush()
        .map_err(|error| format!("failed to flush memory: {error}"))?;
    Ok(true)
}

fn memory_lock_path(path: &Path) -> PathBuf {
    let mut lock_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "memory".into());
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

fn acquire_memory_lock(
    path: &Path,
    cancel: &CancelToken,
) -> Result<Option<ExclusiveFileLock>, String> {
    let lock_path = memory_lock_path(path);
    let deadline = Instant::now() + MEMORY_LOCK_WAIT_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out acquiring memory lock after {}s",
                MEMORY_LOCK_WAIT_TIMEOUT.as_secs()
            ));
        }
        match ExclusiveFileLock::try_acquire(&lock_path) {
            Ok(lock) => return Ok(Some(lock)),
            Err(PlatformError::LockContended { .. }) => thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(format!("failed to acquire memory lock: {error}")),
        }
    }
}

fn format_messages_for_memory(messages: &[Message]) -> String {
    const MAX_BYTES: usize = 32 * 1024;
    let mut output = String::new();
    for message in messages.iter().rev().take(40).rev() {
        match message {
            Message::System { .. } => {}
            Message::User { content, .. } => {
                output.push_str("[user]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
            Message::Assistant { content, .. } => {
                if let Some(content) = content.as_deref().filter(|text| !text.trim().is_empty()) {
                    output.push_str("[assistant]\n");
                    output.push_str(content.trim());
                    output.push_str("\n\n");
                }
            }
            Message::Tool { content, .. } => {
                output.push_str("[tool]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
        }
        if output.len() >= MAX_BYTES {
            break;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_platform::fs::ExclusiveFileLock;
    use tempfile::TempDir;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn memory_block_formats_prompt() {
        let block = MemoryBlock {
            user: Some("prefers concise output".to_string()),
            project: Some("use cargo test".to_string()),
        };
        let prompt = block.to_system_prompt_block().unwrap();
        assert!(prompt.contains("<user>"));
        assert!(prompt.contains("prefers concise output"));
        assert!(prompt.contains("<project>"));
    }

    #[test]
    fn append_note_writes_bullet() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("memory.md");
        append_note(&path, "remember this").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "- remember this\n");
    }

    #[test]
    fn format_messages_for_memory_skips_system_messages() {
        let messages = vec![
            Message::system("system".to_string()),
            Message::user("remember cargo test".to_string()),
        ];
        let formatted = format_messages_for_memory(&messages);
        assert!(!formatted.contains("system"));
        assert!(formatted.contains("remember cargo test"));
    }

    #[test]
    fn auto_memory_provider_config_uses_auxiliary_model_without_tools() {
        let mut config = config();
        config.api_key = Some("key".to_string());
        config.base_url = Some("https://example.test".to_string());

        let provider_config = auto_memory_provider_config(&config);

        assert_eq!(provider_config.api_key.as_deref(), Some("key"));
        assert_eq!(
            provider_config.base_url.as_deref(),
            Some("https://example.test")
        );
        assert_eq!(
            provider_config.model.as_deref(),
            Some(model::auxiliary_model())
        );
        assert!(matches!(provider_config.tools_override, Some(ref tools) if tools.is_empty()));
        assert!(provider_config.mcp_registry.is_none());
        assert!(provider_config.external_tools.is_empty());
    }

    #[test]
    fn cancelled_memory_extraction_does_not_call_provider_or_write_memory() {
        let dir = TempDir::new().unwrap();
        let mut conversation = Conversation::new();
        conversation.add_user("remember cargo test".to_string());
        let cancel = CancelToken::new();
        cancel.cancel();

        let result = extract_project_memory_with_cancel(
            ProviderKind::Mock,
            &auto_memory_provider_config(&config()),
            dir.path(),
            &conversation.messages,
            &cancel,
        )
        .expect("cancelled extraction");

        assert!(result.is_none());
    }

    #[test]
    fn final_response_memory_extraction_honors_turn_cancellation() {
        let dir = TempDir::new().unwrap();
        let mut conversation = Conversation::new();
        conversation.add_user("remember the runtime ownership boundary".to_string());
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut events = EventFactory::new("memory-cancelled-final-response".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);

        extract_project_memory_after_final_response(
            &config(),
            dir.path(),
            &conversation.messages,
            &cancel,
            &mut events,
            &mut sink,
        )
        .expect("cancelled final-response extraction");

        assert!(!dir.path().join("memory").exists());
        assert!(sink.writer_mut().is_empty());
    }

    #[test]
    fn memory_append_waits_for_the_shared_file_lock() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("memory.md");
        let lock = ExclusiveFileLock::acquire(&memory_lock_path(&path)).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            append_note(&writer_path, "serialized note").unwrap();
            done_tx.send(()).unwrap();
        });

        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(lock);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("append completes after lock release");
        writer.join().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "- serialized note\n");
    }

    #[test]
    fn cancelled_memory_append_releases_without_persisting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("memory.md");
        let lock = ExclusiveFileLock::acquire(&memory_lock_path(&path)).unwrap();
        let cancel = CancelToken::new();
        let writer_cancel = cancel.clone();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            append_note_with_cancel(&writer_path, "cancelled note", &writer_cancel).unwrap()
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel.cancel();
        assert!(!writer.join().unwrap());
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn concurrent_memory_appends_retain_each_complete_record() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("memory.md");
        let writers = (0..16)
            .map(|index| {
                let writer_path = path.clone();
                std::thread::spawn(move || {
                    append_note(&writer_path, &format!("concurrent note {index}"))
                        .expect("append concurrent note");
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().unwrap();
        }

        let mut lines = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        lines.sort();
        let mut expected = (0..16)
            .map(|index| format!("- concurrent note {index}"))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(lines, expected);
    }
}
