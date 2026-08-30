use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::thread_store::sessions_dir;
use orca_core::config::{ActivePermissionProfile, AdditionalWorkingDirectory};
use orca_core::conversation::{Conversation, Message, SummaryState, normalize_tool_boundaries};
use orca_core::{approval_rules::PermissionRules, approval_types::ApprovalMode};

pub use crate::thread_store::{
    JsonlThreadStore, LiveThread, SearchHit, SessionMeta, SessionStore, SessionSummary,
    SessionSummaryPage, SessionTranscript, SessionWriter, SortDirection, StoredSessionHealth,
    StoredSessionHealthIssue, StoredThreadItem, StoredThreadItemPage, StoredThreadProjection,
    StoredThreadSearchDiagnostic, StoredThreadSearchHit, StoredThreadSearchPage,
    StoredThreadSummary, StoredThreadSummaryPage, StoredThreadTurn, StoredThreadTurnPage,
    ThreadListFilters, ThreadMetadataPatch, ThreadRelationFilter, ThreadSortKey, ThreadStore,
    TurnItemsView, archive_session, compress_session, delete_session, list_session_page,
    list_sessions, list_sessions_with_archived, load_session, rename_session, search_sessions,
};

const SESSION_SCHEMA_VERSION: u32 = 1;

/// Serializes tests that deliberately share process-wide state (the session
/// index, the task registry index, recorded sessions under the process-wide
/// isolated home). This is the v0.3.14 serialization behavior: the shared
/// index stores are safe under serial execution, so tests that touch them
/// hold this mutex for their whole body. Home resolution itself never takes
/// this lock (see [`read_test_orca_home`] and [`with_test_orca_home`]), so a
/// test holding it while waiting for its own host threads can never deadlock
/// against them.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn recover_test_lock(mutex: &'static std::sync::Mutex<()>) -> std::sync::MutexGuard<'static, ()> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
    recover_test_lock(&TEST_ENV_LOCK)
}

// Per-test `ORCA_HOME` overrides live in `orca_core::config::folder_trust`
// (thread-local, never the environment) so that folder-trust and workflow
// script resolution observe the same private home as sessions, goals, and
// task sessions. Hosts spawned inside a test-home scope capture it
// (`current_test_orca_home`) and install it on all their worker threads via
// `set_host_orca_home`.

#[cfg(test)]
pub(crate) fn current_test_orca_home() -> Option<std::path::PathBuf> {
    orca_core::config::folder_trust::current_test_orca_home()
}

/// Installs `home` as the `ORCA_HOME` override for the calling thread (a
/// host supervisor or worker thread). `None` restores environment-based
/// resolution.
#[cfg(test)]
pub(crate) fn set_host_orca_home(home: Option<std::path::PathBuf>) {
    orca_core::config::folder_trust::install_host_orca_home(home);
}

/// Runs `body` with `ORCA_HOME` resolving to `home` for the calling thread
/// and for any host spawned inside the closure, then restores the previous
/// resolution — even when `body` panics. `ORCA_HOME` itself is never
/// mutated: the override is a thread-local that other tests cannot observe,
/// so concurrent tests, background hosts, and server threads always resolve
/// the process-wide home and never see a half-switched value.
#[cfg(test)]
pub(crate) fn with_test_orca_home<T>(
    home: &std::path::Path,
    body: impl FnOnce(&std::path::Path) -> T,
) -> T {
    let previous = current_test_orca_home();
    orca_core::config::folder_trust::install_test_orca_home(Some(home.to_path_buf()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(home)));
    orca_core::config::folder_trust::install_test_orca_home(previous);
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Guard returned by [`redirect_test_orca_home`]; restores the previous
/// per-test home override on drop.
#[cfg(test)]
pub(crate) struct TestOrcaHomeGuard {
    previous: Option<std::path::PathBuf>,
}

#[cfg(test)]
impl Drop for TestOrcaHomeGuard {
    fn drop(&mut self) {
        orca_core::config::folder_trust::install_test_orca_home(self.previous.take());
    }
}

/// Redirects `ORCA_HOME` resolution to `home` for the calling thread until
/// the returned guard drops (the guard restores the previous override).
/// Unlike [`with_test_orca_home`], the caller keeps the override alive
/// across statements, which tests with multi-phase bodies (host setup,
/// assertions, resume) need. The redirect is a thread-local and is invisible
/// to every other test.
#[cfg(test)]
pub(crate) fn redirect_test_orca_home(home: &std::path::Path) -> TestOrcaHomeGuard {
    let previous = current_test_orca_home();
    orca_core::config::folder_trust::install_test_orca_home(Some(home.to_path_buf()));
    TestOrcaHomeGuard { previous }
}

/// Reads `ORCA_HOME` safely under test. The resolution order is: this
/// thread's host-home override (installed on host worker threads), this
/// thread's test-home override (inside a `with_test_orca_home` /
/// `redirect_test_orca_home` scope), then the process-wide `ORCA_HOME`
/// variable. No lock is taken: the variable is only ever set to the
/// process-wide isolated home, so reads never observe a half-switched value.
#[cfg(test)]
pub(crate) fn read_test_orca_home() -> Option<std::ffi::OsString> {
    orca_core::config::folder_trust::current_orca_home_override()
        .map(Into::into)
        .or_else(|| std::env::var_os("ORCA_HOME"))
}

/// One process-wide temporary `ORCA_HOME` for the whole lib test binary. The
/// real user home may be in use by live `orca` processes, so every lib test
/// resolves `ORCA_HOME` to this isolated directory instead. It is created
/// once and never removed while the test process runs; this function does not
/// touch the environment.
#[cfg(test)]
pub(crate) fn isolated_test_orca_home_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;

    static ORCA_HOME_TEST_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let home = ORCA_HOME_TEST_HOME
        .get_or_init(|| tempfile::tempdir().expect("create process-wide test ORCA_HOME"));
    home.path()
}

/// Re-applies `ORCA_HOME` to the process-wide isolated home and returns it.
/// Safe to call from anywhere: nothing else ever mutates the variable (all
/// per-test homes are thread-local overrides), so this is idempotent.
#[cfg(test)]
pub(crate) fn isolated_test_orca_home() -> &'static std::path::Path {
    let home = isolated_test_orca_home_dir();
    // edition 2024: set_var is unsafe.
    unsafe {
        std::env::set_var("ORCA_HOME", home);
    }
    home
}

/// Ensures the process-wide isolated ORCA_HOME directory exists for tests
/// that run without any explicit home. The variable is stable for the whole
/// test process (per-test homes are thread-local), so a claim simply
/// re-applies the same value. Recovery children receive an explicit fixture
/// ORCA_HOME on their command line and keep it.
#[cfg(test)]
pub(crate) fn claim_isolated_test_orca_home_if_unset() -> bool {
    if std::env::var_os("ORCA_HOME").is_some() {
        return false;
    }
    let _ = isolated_test_orca_home();
    true
}

/// Reserves a unique subdirectory inside the process-wide isolated home for a
/// test that needs an exclusive empty home (for example to create a repository
/// or write trust receipts). The subdirectory is never removed and is never
/// exported through `ORCA_HOME`: tests that only need a private fixture
/// directory receive it as an explicit path and leave the environment alone.
/// Allocation is atomic (a process-wide counter), so concurrent tests never
/// receive the same subdirectory.
#[cfg(test)]
pub(crate) fn isolated_test_orca_home_subdir(label: &str) -> std::path::PathBuf {
    static SUBDIR_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let home = isolated_test_orca_home_dir();
    let sequence = SUBDIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let candidate = home.join(format!("{label}-{}-{}", std::process::id(), sequence));
    std::fs::create_dir_all(&candidate).expect("create isolated test home subdir");
    candidate
}

/// Reserves an exclusive never-removed subdirectory of the process-wide
/// isolated home and runs `body` with `ORCA_HOME` resolving to it for the
/// calling thread (and any host spawned inside), restoring the previous
/// resolution afterwards — even on panic. Use this for tests whose fixture
/// must be visible through the home itself (a broken legacy goal file, a
/// transcript directory replaced with a file, task-registry migrations).
#[cfg(test)]
pub(crate) fn with_redirected_orca_home<T>(
    label: &str,
    body: impl FnOnce(&std::path::Path) -> T,
) -> T {
    let home = isolated_test_orca_home_subdir(label);
    with_test_orca_home(&home, body)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactionRecord {
    pub collapsed_at: DateTime<Utc>,
    pub before_messages: usize,
    pub after_messages: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContextSummaryRecord {
    pub summarized_at: DateTime<Utc>,
    pub before_messages: usize,
    pub after_messages: usize,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_state: Option<SummaryState>,
}

impl JsonlThreadStore {
    pub fn new() -> Self {
        Self
    }

    pub fn create_meta(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> SessionMeta {
        create_meta(cwd, provider, model, prompt)
    }

    pub fn create_meta_with_permissions(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_mode: ApprovalMode,
        permission_rules: PermissionRules,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
    ) -> SessionMeta {
        let mut meta = create_meta(cwd, provider, model, prompt);
        meta.active_permission_profile = active_permission_profile;
        meta.approval_mode = Some(approval_mode);
        meta.runtime_workspace_roots = vec![cwd.to_path_buf()];
        meta.permission_rules = permission_rules;
        meta.additional_working_directories = additional_working_directories;
        meta
    }

    pub fn create_fork_meta(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        parent_id: String,
    ) -> SessionMeta {
        create_fork_meta(cwd, provider, model, prompt, parent_id)
    }

    pub fn start_writer(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<SessionWriter> {
        SessionWriter::start(cwd, provider, model, prompt)
    }

    pub fn start_writer_from_meta(&self, meta: SessionMeta) -> io::Result<SessionWriter> {
        SessionWriter::start_from_meta(meta)
    }

    pub fn resume_conversation(
        &self,
        transcript: &SessionTranscript,
        system_prompt: String,
    ) -> Conversation {
        resume_conversation(transcript, system_prompt)
    }
}

pub fn resume_conversation(transcript: &SessionTranscript, system_prompt: String) -> Conversation {
    let mut conversation = Conversation::new();
    conversation.add_system(system_prompt);
    let mut restored_messages = replay_compactions_for_resume(
        &transcript.messages,
        &transcript.compactions,
        &transcript.summaries,
    )
    .into_iter()
    .filter(|message| {
        !matches!(
            message,
            Message::System {
                content,
                pinned: false,
            } if !content.starts_with(
                "[Earlier conversation history was truncated to fit context window]"
            )
        )
    })
    .collect::<Vec<_>>();
    normalize_tool_boundaries(&mut restored_messages);
    for message in restored_messages.iter() {
        conversation.messages.push(message.clone());
    }
    if let Some(summary_state) = transcript
        .summaries
        .iter()
        .rev()
        .find_map(|record| record.summary_state.clone())
    {
        conversation.summary = summary_state;
        conversation.rolling_summary = transcript
            .summaries
            .last()
            .map(|record| record.summary.clone());
    } else if let Some(first_summary) = transcript.summaries.first() {
        conversation.summary.baseline = Some(first_summary.summary.clone());
        conversation.summary.deltas = transcript
            .summaries
            .iter()
            .skip(1)
            .map(|record| record.summary.clone())
            .collect();
        conversation.rolling_summary = transcript
            .summaries
            .last()
            .map(|record| record.summary.clone());
    }
    conversation
}

fn replay_compactions_for_resume(
    messages: &[Message],
    compactions: &[CompactionRecord],
    summaries: &[ContextSummaryRecord],
) -> Vec<Message> {
    let mut summarized_compactions = HashMap::<(usize, usize), usize>::new();
    for summary in summaries {
        *summarized_compactions
            .entry((summary.before_messages, summary.after_messages))
            .or_default() += 1;
    }
    let mut restored = messages.to_vec();
    for compaction in compactions {
        let has_remote_summary = summarized_compactions
            .get_mut(&(compaction.before_messages, compaction.after_messages))
            .is_some_and(|remaining| {
                if *remaining == 0 {
                    return false;
                }
                *remaining -= 1;
                true
            });
        restored = replay_compaction_for_resume(restored, compaction, has_remote_summary);
    }
    restored
}

fn replay_compaction_for_resume(
    messages: Vec<Message>,
    compaction: &CompactionRecord,
    has_remote_summary: bool,
) -> Vec<Message> {
    if compaction.before_messages == 0
        || compaction.after_messages >= compaction.before_messages
        || messages.len() < compaction.before_messages
    {
        return messages;
    }

    let prefix = &messages[..compaction.before_messages];
    let suffix = &messages[compaction.before_messages..];
    let system = prefix
        .iter()
        .find(|message| matches!(message, Message::System { .. }))
        .cloned();
    let pinned: Vec<Message> = prefix
        .iter()
        .filter(|message| !matches!(message, Message::System { .. }) && message.is_pinned())
        .cloned()
        .collect();

    let structural_messages = usize::from(system.is_some()) + usize::from(!has_remote_summary);
    let retained_non_system = compaction
        .after_messages
        .saturating_sub(structural_messages);
    let retained_tail = retained_non_system.saturating_sub(pinned.len());
    let mut tail: Vec<Message> = prefix
        .iter()
        .filter(|message| !matches!(message, Message::System { .. }) && !message.is_pinned())
        .rev()
        .take(retained_tail)
        .cloned()
        .collect();
    tail.reverse();

    let mut replayed = Vec::with_capacity(
        usize::from(system.is_some()) + pinned.len() + tail.len() + suffix.len(),
    );
    if let Some(system) = system {
        replayed.push(system);
    }
    replayed.extend(pinned);
    replayed.extend(tail);
    replayed.extend_from_slice(suffix);
    replayed
}

pub fn create_meta(cwd: &Path, provider: &str, model: Option<String>, prompt: &str) -> SessionMeta {
    let now = Utc::now();
    SessionMeta {
        schema_version: SESSION_SCHEMA_VERSION,
        session_id: Uuid::new_v4().to_string(),
        cwd: cwd.display().to_string(),
        provider: provider.to_string(),
        model,
        title: title_from_prompt(prompt),
        created_at: now,
        parent_id: None,
        forked: false,
        approval_mode: None,
        active_permission_profile: None,
        runtime_workspace_roots: vec![cwd.to_path_buf()],
        permission_rules: PermissionRules::default(),
        additional_working_directories: Vec::new(),
        metadata_writable_directories: Vec::new(),
        network_domain_permissions: Default::default(),
    }
}

pub fn create_fork_meta(
    cwd: &Path,
    provider: &str,
    model: Option<String>,
    prompt: &str,
    parent_id: String,
) -> SessionMeta {
    let mut meta = create_meta(cwd, provider, model, prompt);
    meta.parent_id = Some(parent_id);
    meta.forked = true;
    meta
}

pub(crate) fn session_path(session_id: &str, timestamp: DateTime<Utc>) -> io::Result<PathBuf> {
    let path = prospective_session_path(session_id, timestamp);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    Ok(path)
}

pub(crate) fn prospective_session_path(session_id: &str, timestamp: DateTime<Utc>) -> PathBuf {
    let dir = sessions_dir()
        .join(format!("{:04}", timestamp.year()))
        .join(format!("{:02}", timestamp.month()))
        .join(format!("{:02}", timestamp.day()));
    dir.join(format!(
        "session-{}-{}.jsonl",
        timestamp.format("%Y-%m-%dT%H-%M-%S"),
        session_id
    ))
}

fn title_from_prompt(prompt: &str) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "(empty prompt)".to_string();
    }
    const MAX_CHARS: usize = 80;
    let mut title: String = normalized.chars().take(MAX_CHARS).collect();
    if normalized.chars().count() > MAX_CHARS {
        title.push_str("...");
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::conversation::RawToolCall;
    use orca_core::event_schema::{
        EVENT_SEQUENCE_RESERVATION_SIZE, EventEnvelope, EventPublicationStore, EventType,
    };

    /// Points ORCA_HOME resolution at an exclusive never-removed subdirectory
    /// of the process-wide isolated home for the duration of `body`. The
    /// override is a thread-local (never the environment), so concurrent
    /// tests keep resolving the process-wide home.
    fn with_isolated_home<T>(body: impl FnOnce() -> T) -> T {
        let home = isolated_test_orca_home_subdir("history-test");
        super::with_test_orca_home(&home, |_| body())
    }
    use orca_core::plan_types::{PlanItem, PlanStatus};
    use orca_core::thread_identity::TurnId;
    use orca_core::tool_types::{
        ToolName, ToolRequest, ToolResult, ToolStatus, ToolTerminalSource,
    };

    #[test]
    fn title_from_prompt_normalizes_whitespace_and_truncates() {
        assert_eq!(title_from_prompt(" hello\nworld "), "hello world");
        assert_eq!(title_from_prompt("   "), "(empty prompt)");
        assert!(title_from_prompt(&"x".repeat(100)).ends_with("..."));
    }

    #[test]
    fn test_env_lock_recovers_after_poisoned_test_panic() {
        static LOCAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        let poisoned = std::thread::spawn(|| {
            let _guard = recover_test_lock(&LOCAL_LOCK);
            panic!("poison a test lock");
        })
        .join();
        assert!(poisoned.is_err());

        drop(recover_test_lock(&LOCAL_LOCK));
    }

    #[test]
    fn writer_persists_compaction_records() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "compact me")?;
            writer.append_compaction(42, 7)?;
            writer.append_summary(42, 7, "important facts")?;
            let transcript = load_session("latest")?;
            assert_eq!(transcript.compactions.len(), 1);
            assert_eq!(transcript.compactions[0].before_messages, 42);
            assert_eq!(transcript.compactions[0].after_messages, 7);
            assert_eq!(transcript.summaries.len(), 1);
            assert_eq!(transcript.summaries[0].summary, "important facts");
            Ok::<(), io::Error>(())
        });
        result.expect("compaction record persisted");
    }

    #[test]
    fn event_publication_state_survives_rewrite_compression_and_restore() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let writer = SessionWriter::start(&cwd, "mock", None, "durable sequence")?;
            let session_id = load_session("latest")?.meta.session_id;
            writer.reserve_through(EVENT_SEQUENCE_RESERVATION_SIZE)?;
            let semantic_event = EventEnvelope {
                version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
                run_id: session_id.clone(),
                seq: 7,
                timestamp_ms: 77,
                event_type: EventType::Error,
                payload: serde_json::json!({ "message": "durable semantic event" }),
            };
            writer.append_semantic_event(&semantic_event)?;

            rename_session(&session_id, "rewritten durable sequence")?;
            let rewritten = load_session(&session_id)?;
            assert_eq!(rewritten.next_event_seq, EVENT_SEQUENCE_RESERVATION_SIZE);
            assert_eq!(
                rewritten.semantic_events.as_slice(),
                std::slice::from_ref(&semantic_event)
            );

            let compressed_path = compress_session(&session_id)?;
            assert_eq!(
                compressed_path
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("zst")
            );
            let compressed = load_session(&session_id)?;
            assert_eq!(compressed.next_event_seq, EVENT_SEQUENCE_RESERVATION_SIZE);
            assert_eq!(compressed.meta.title, "rewritten durable sequence");
            assert_eq!(
                compressed.semantic_events.as_slice(),
                std::slice::from_ref(&semantic_event)
            );

            SessionWriter::append_to_existing(compressed.path.clone())?;
            let restored = load_session(&session_id)?;
            assert_eq!(
                restored
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("jsonl")
            );
            assert_eq!(restored.next_event_seq, EVENT_SEQUENCE_RESERVATION_SIZE);
            assert_eq!(restored.semantic_events, [semantic_event]);
            Ok::<(), io::Error>(())
        });
        result.expect("event publication state survived rewrite, compression, and restore");
    }

    #[test]
    fn writer_round_trips_pinned_messages() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "remember")?;
            writer.enter_turn(TurnId::new());
            writer.append_message(&Message::pinned_user("pinned constraint".to_string()))?;
            let transcript = load_session("latest")?;
            assert_eq!(transcript.messages.len(), 1);
            assert!(transcript.messages[0].is_pinned());
            assert!(matches!(
                &transcript.messages[0],
                Message::User { content, .. } if content == "pinned constraint"
            ));
            Ok::<(), io::Error>(())
        });
        result.expect("pinned message round-tripped");
    }

    #[test]
    fn writer_redacts_secrets_before_persisting_transcript() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let prompt_secret = "sk-test-redaction-title-1234567890";
            let env_secret = "sk-test-redaction-env-1234567890";
            let json_secret = "sk-test-redaction-json-1234567890";
            let password_secret = "super-secret-test-password";
            let tool_secret = "tool-token-test-secret";
            let mut writer = SessionWriter::start(
                &cwd,
                "mock",
                None,
                &format!("start ORCA_API_KEY={prompt_secret}"),
            )?;
            writer.enter_turn(TurnId::new());
            writer.append_message(&Message::user(format!(
                "please run ORCA_API_KEY={env_secret} and password={password_secret}"
            )))?;
            writer.append_message(&Message::Assistant {
                content: Some(format!(
                    "configured with {{\"DEEPSEEK_API_KEY\":\"{json_secret}\"}}"
                )),
                reasoning_content: Some(format!("reasoning token={tool_secret}")),
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "shell".to_string(),
                    arguments: format!("{{\"env\":{{\"API_TOKEN\":\"{tool_secret}\"}}}}"),
                }],
                pinned: false,
            })?;
            let tool_request = ToolRequest {
                id: "call_1".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: None,
                raw_arguments: None,
            };
            let tool_result = ToolResult::failed(
                &tool_request,
                format!("tool failed with token {tool_secret}"),
                Some(1),
            );
            writer.append_message(&Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: format!("TOKEN={tool_secret}"),
                terminal: Some(tool_result.terminal().clone()),
                pinned: false,
            })?;
            writer.append_summary(3, 2, format!("summary kept {json_secret}"))?;
            writer.append_plan_state(
                Some(format!("plan with {env_secret}")),
                vec![PlanItem {
                    step: format!("step uses {password_secret}"),
                    status: PlanStatus::Pending,
                }],
            )?;

            let transcript = load_session("latest")?;
            let raw = fs::read_to_string(&transcript.path)?;
            for secret in [
                prompt_secret,
                env_secret,
                json_secret,
                password_secret,
                tool_secret,
            ] {
                assert!(
                    !raw.contains(secret),
                    "raw transcript leaked secret value {secret}"
                );
            }
            assert!(raw.contains("<redacted>"));
            assert!(raw.contains("please run"));
            assert!(raw.contains("configured with"));

            let rendered_loaded = transcript
                .messages
                .iter()
                .filter_map(Message::content_str)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!rendered_loaded.contains(env_secret));
            assert!(!rendered_loaded.contains(json_secret));
            assert!(rendered_loaded.contains("<redacted>"));

            Ok::<(), io::Error>(())
        });
        result.expect("session transcript secrets redacted");
    }

    #[test]
    fn plan_state_round_trips_through_session() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "plan test")?;
            writer.append_plan_state(
                Some("initial plan".to_string()),
                vec![
                    PlanItem {
                        step: "Step 1".to_string(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        step: "Step 2".to_string(),
                        status: PlanStatus::InProgress,
                    },
                    PlanItem {
                        step: "Step 3".to_string(),
                        status: PlanStatus::Pending,
                    },
                ],
            )?;
            let transcript = load_session("latest")?;
            let (explanation, plan) = transcript.plan.expect("plan should be present");
            assert_eq!(explanation.as_deref(), Some("initial plan"));
            assert_eq!(plan.len(), 3);
            assert_eq!(plan[0].step, "Step 1");
            assert_eq!(plan[1].status, PlanStatus::InProgress);
            Ok::<(), io::Error>(())
        });
        result.expect("plan state round-tripped");
    }

    #[test]
    fn all_completed_plan_restores_as_none() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "done plan")?;
            writer.append_plan_state(
                None,
                vec![
                    PlanItem {
                        step: "Done 1".to_string(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        step: "Done 2".to_string(),
                        status: PlanStatus::Completed,
                    },
                ],
            )?;
            let transcript = load_session("latest")?;
            assert!(
                transcript.plan.is_none(),
                "all-completed plan should restore as None"
            );
            Ok::<(), io::Error>(())
        });
        result.expect("all-completed plan cleared");
    }

    #[test]
    fn empty_plan_restores_as_none() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "empty plan")?;
            writer.append_plan_state(None, vec![])?;
            let transcript = load_session("latest")?;
            assert!(
                transcript.plan.is_none(),
                "empty plan should restore as None"
            );
            Ok::<(), io::Error>(())
        });
        result.expect("empty plan cleared");
    }

    #[test]
    fn session_without_plan_loads_normally() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let _writer = SessionWriter::start(&cwd, "mock", None, "no plan")?;
            let transcript = load_session("latest")?;
            assert!(
                transcript.plan.is_none(),
                "no plan records means plan is None"
            );
            assert_eq!(transcript.meta.title, "no plan");
            Ok::<(), io::Error>(())
        });
        result.expect("session without plan loaded");
    }

    #[test]
    fn resume_restores_rolling_summary_from_last_context_summary_record() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "rolling summary")?;
            writer.append_summary(10, 5, "first summary")?;
            writer.append_summary(20, 8, "updated rolling summary")?;
            let transcript = load_session("latest")?;

            let conv = resume_conversation(&transcript, "new system prompt".to_string());
            assert_eq!(
                conv.rolling_summary.as_deref(),
                Some("updated rolling summary"),
                "should restore the last summary as rolling_summary"
            );
            assert_eq!(
                conv.summary.baseline.as_deref(),
                Some("first summary"),
                "first summary record should remain the stable baseline"
            );
            assert_eq!(
                conv.summary.deltas,
                vec!["updated rolling summary".to_string()],
                "later summary records should resume as append-only deltas"
            );
            Ok::<(), io::Error>(())
        });
        result.expect("rolling summary restored from history");
    }

    #[test]
    fn resume_without_summaries_has_no_rolling_summary() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let _writer = SessionWriter::start(&cwd, "mock", None, "no summaries")?;
            let transcript = load_session("latest")?;

            let conv = resume_conversation(&transcript, "sys".to_string());
            assert!(
                conv.rolling_summary.is_none(),
                "no summary records means no rolling_summary"
            );
            Ok::<(), io::Error>(())
        });
        result.expect("no rolling summary without records");
    }

    #[test]
    fn resume_preserves_manual_compaction_marker_and_pinned_hook_context() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "manual snapshot")?;
            let operation_id = crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .expect("generated UUID is v7");
            let identity = crate::session::ManualCompactionPersistenceIdentity {
                operation_id,
                snapshot_id: uuid::Uuid::now_v7().to_string(),
            };
            let mut compacted = Conversation::new();
            compacted.add_system(
                "[Earlier conversation history was truncated to fit context window]".to_string(),
            );
            compacted.add_system_pinned("[Hook context]\nretained".to_string());
            compacted.add_user("tail".to_string());
            writer.append_manual_compaction_snapshot(
                &identity,
                5,
                "local_truncation",
                &compacted,
            )?;

            let transcript = load_session("latest")?;
            let resumed = resume_conversation(&transcript, "fresh system".to_string());
            assert!(matches!(
                resumed.messages.as_slice(),
                [
                    Message::System { content: system, .. },
                    Message::System { content: marker, pinned: false },
                    Message::System { content: hook, pinned: true },
                    Message::User { content: tail, .. },
                ] if system == "fresh system"
                    && marker
                        == "[Earlier conversation history was truncated to fit context window]"
                    && hook == "[Hook context]\nretained"
                    && tail == "tail"
            ));
            Ok::<(), io::Error>(())
        });
        result.expect("manual compaction system context resumes");
    }

    #[test]
    fn resume_repairs_incomplete_assistant_tool_call_turns() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let meta = create_meta(&cwd, "mock", None, "bad tool boundary");
            let session_id = meta.session_id.clone();
            let mut writer = SessionWriter::start_from_meta(meta)?;
            writer.enter_turn(TurnId::new());
            writer.append_message(&Message::user("start".to_string()))?;
            writer.append_message(&Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: "{\"path\":\"README.md\"}".to_string(),
                }],
                pinned: false,
            })?;
            writer.enter_turn(TurnId::new());
            writer.append_message(&Message::user("continue after failed turn".to_string()))?;

            let transcript = load_session(&session_id)?;
            let path = transcript.path.clone();
            let original = fs::read(&path)?;
            let conv = resume_conversation(&transcript, "sys".to_string());

            assert_eq!(conv.messages.len(), 5);
            assert!(matches!(
                &conv.messages[2],
                Message::Assistant { tool_calls, .. } if tool_calls.len() == 1
            ));
            assert!(matches!(
                &conv.messages[3],
                Message::Tool { tool_call_id, terminal: Some(terminal), .. }
                    if tool_call_id == "call_1"
                        && terminal.status == ToolStatus::Indeterminate
                        && terminal.source == ToolTerminalSource::CompatibilityRepair
            ));
            assert!(matches!(
                &conv.messages[4],
                Message::User { content, .. } if content == "continue after failed turn"
            ));
            assert_eq!(
                fs::read(&path)?,
                original,
                "resume repair must not rewrite the source JSONL"
            );
            Ok::<(), io::Error>(())
        });
        result.expect("resume repaired real JSONL without rewriting it");
    }

    #[test]
    fn legacy_tool_result_metadata_without_new_terminal_fields_remains_readable() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let meta = create_meta(&cwd, "mock", None, "legacy tool metadata");
            let session_id = meta.session_id.clone();
            let path = session_path(&meta.session_id, meta.created_at)?;
            let writer = SessionWriter::start_from_meta(meta)?;
            drop(writer);
            let legacy = serde_json::json!({
                "type": "conversation.message",
                "message": {
                    "role": "tool",
                    "tool_call_id": "legacy-call",
                    "content": "ERROR: boom",
                    "status": "failed",
                    "error": "boom",
                    "exit_code": 42,
                    "truncated": true
                }
            });
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new().append(true).open(&path)?;
            writeln!(file, "{legacy}")?;

            let transcript = load_session(&session_id)?;
            assert!(matches!(
                transcript.messages.last(),
                Some(Message::Tool { terminal: Some(terminal), .. })
                    if terminal.status == ToolStatus::Failed
                        && terminal.error.as_deref() == Some("boom")
                        && terminal.exit_code == Some(42)
                        && terminal.truncated
                        && terminal.source == ToolTerminalSource::Observed
            ));
            Ok::<(), io::Error>(())
        });
        result.expect("legacy tool terminal metadata remained readable");
    }

    #[test]
    fn legacy_tool_result_metadata_without_status_becomes_indeterminate_repair() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let meta = create_meta(&cwd, "mock", None, "legacy partial tool metadata");
            let session_id = meta.session_id.clone();
            let path = session_path(&meta.session_id, meta.created_at)?;
            let writer = SessionWriter::start_from_meta(meta)?;
            drop(writer);
            let legacy = serde_json::json!({
                "type": "conversation.message",
                "message": {
                    "role": "tool",
                    "tool_call_id": "legacy-call",
                    "content": "legacy output",
                    "error": "legacy diagnostic",
                    "exit_code": 42,
                    "truncated": true
                }
            });
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new().append(true).open(&path)?;
            writeln!(file, "{legacy}")?;

            let transcript = load_session(&session_id)?;
            assert!(matches!(
                transcript.messages.last(),
                Some(Message::Tool { terminal: Some(terminal), .. })
                    if terminal.status == ToolStatus::Indeterminate
                        && terminal.error.as_deref() == Some("legacy diagnostic")
                        && terminal.exit_code == Some(42)
                        && terminal.truncated
                        && terminal.source == ToolTerminalSource::CompatibilityRepair
            ));
            Ok::<(), io::Error>(())
        });
        result.expect("legacy status-less tool diagnostics were repaired conservatively");
    }

    #[test]
    fn resume_drops_reasoning_only_assistant() {
        let cwd = std::env::current_dir().unwrap();
        let transcript = SessionTranscript {
            meta: create_meta(&cwd, "deepseek", None, "reasoning-only turn"),
            messages: vec![
                Message::user("first".to_string()),
                Message::Assistant {
                    content: None,
                    reasoning_content: Some("private thinking".to_string()),
                    tool_calls: vec![],
                    pinned: false,
                },
                Message::user("second".to_string()),
            ],
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: None,
            plan: None,
            completion_status: None,
            completion_error: None,
            next_event_seq: 0,
            semantic_events: Vec::new(),
            path: cwd.join("reasoning-only.jsonl"),
        };

        let conv = resume_conversation(&transcript, "sys".to_string());

        assert!(!conv.messages.iter().any(|message| matches!(
            message,
            Message::Assistant {
                content: None,
                tool_calls,
                ..
            } if tool_calls.is_empty()
        )));
        assert!(conv.messages.iter().any(|message| matches!(
            message,
            Message::User { content, .. } if content == "second"
        )));
    }

    #[test]
    fn load_session_preserves_latest_completion_status_and_redacted_error() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "failed turn")?;
            writer.complete_with_error(
                "failed",
                Some("DeepSeek provider error: api_key=super-secret"),
            )?;
            let transcript = load_session("latest")?;

            assert_eq!(transcript.completion_status.as_deref(), Some("failed"));
            assert_eq!(
                transcript.completion_error.as_deref(),
                Some("DeepSeek provider error: api_key=<redacted>")
            );
            Ok::<(), io::Error>(())
        });
        result.expect("completion status and error restored from history");
    }

    #[test]
    fn resume_prefers_persisted_summary_state_over_legacy_summary_list() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "summary state")?;
            writer.append_summary(10, 5, "legacy baseline")?;
            writer.append_summary(20, 8, "legacy delta")?;
            writer.append_summary_state(
                30,
                9,
                "new delta",
                &SummaryState {
                    baseline: Some("rebuilt baseline".to_string()),
                    deltas: vec!["fresh delta".to_string()],
                },
            )?;
            let transcript = load_session("latest")?;

            let conv = resume_conversation(&transcript, "sys".to_string());
            assert_eq!(
                conv.summary.baseline.as_deref(),
                Some("rebuilt baseline"),
                "latest persisted summary_state should be exact resume source"
            );
            assert_eq!(conv.summary.deltas, vec!["fresh delta".to_string()]);
            assert_eq!(conv.rolling_summary.as_deref(), Some("new delta"));
            Ok::<(), io::Error>(())
        });
        result.expect("summary state restored from history");
    }

    #[test]
    fn resume_replays_compaction_records_to_drop_collapsed_messages() {
        let result = with_isolated_home(|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "compacted resume")?;
            writer.append_message(&Message::system("old system".to_string()))?;
            writer.enter_turn(TurnId::new());
            writer.append_message(&Message::user("collapsed old user".repeat(100)))?;
            writer.append_message(&Message::Assistant {
                content: Some("collapsed old assistant".repeat(100)),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })?;
            writer.append_message(&Message::Assistant {
                content: Some("kept tail before compaction".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })?;
            writer.append_compaction(4, 2)?;
            writer.append_summary_state(
                4,
                2,
                "summary of collapsed old messages",
                &SummaryState {
                    baseline: Some("summary baseline".to_string()),
                    deltas: Vec::new(),
                },
            )?;
            writer.enter_turn(TurnId::new());
            writer.append_message(&Message::user("new prompt after compaction".to_string()))?;

            let transcript = load_session("latest")?;
            let conv = resume_conversation(&transcript, "fresh system".to_string());
            let rendered = conv
                .messages
                .iter()
                .filter_map(Message::content_str)
                .collect::<Vec<_>>()
                .join("\n");

            assert!(
                !rendered.contains("collapsed old user"),
                "collapsed pre-compaction user message should not re-enter resumed context"
            );
            assert!(
                !rendered.contains("collapsed old assistant"),
                "collapsed pre-compaction assistant message should not re-enter resumed context"
            );
            assert!(rendered.contains("kept tail before compaction"));
            assert!(rendered.contains("new prompt after compaction"));
            assert_eq!(conv.summary.baseline.as_deref(), Some("summary baseline"));
            Ok::<(), io::Error>(())
        });
        result.expect("compacted messages filtered on resume");
    }

    #[test]
    fn repeated_compactions_consume_matching_summaries_once() {
        let messages = vec![
            Message::system("system".to_string()),
            Message::user("old-a".to_string()),
            Message::Assistant {
                content: Some("old-b".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            },
            Message::user("old-c".to_string()),
            Message::Assistant {
                content: Some("old-d".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            },
            Message::user("old-e".to_string()),
        ];
        let compactions = vec![
            CompactionRecord {
                collapsed_at: Utc::now(),
                before_messages: 4,
                after_messages: 2,
            },
            CompactionRecord {
                collapsed_at: Utc::now(),
                before_messages: 4,
                after_messages: 2,
            },
        ];
        let summaries = vec![ContextSummaryRecord {
            summarized_at: Utc::now(),
            before_messages: 4,
            after_messages: 2,
            summary: "only the first compaction was summarized".to_string(),
            summary_state: None,
        }];

        let restored = replay_compactions_for_resume(&messages, &compactions, &summaries);
        let rendered = restored
            .iter()
            .filter_map(Message::content_str)
            .collect::<Vec<_>>();

        assert_eq!(rendered, vec!["system"]);
    }
}
