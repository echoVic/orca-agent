//! `/rewind` runtime integration: per-turn pre-image capture and the
//! coordinator that lists checkpoints and restores files / conversation.
//!
//! The durable records live in the session transcript (`session.snapshot`,
//! `session.rewind`) and the file contents live in `orca_rewind`'s
//! content-addressed store under `$ORCA_HOME/rewind/sessions/<session_id>`.
//! This module only wires those two together:
//!
//! * [`RewindTurn`] — the per-turn capture. It is registered in the thread's
//!   [`ExtensionData`](crate::extension::ExtensionData) so the tool dispatch
//!   path can capture a file's pre-image before an `edit` / `write_file`
//!   actually runs. Capture is best-effort: a failure records a limitation
//!   and never blocks the tool.
//! * [`list_checkpoints`] / [`apply_rewind`] — the coordinator behind
//!   `InteractiveSession::{rewind_checkpoints,rewind_to_checkpoint}` and the
//!   runtime host commands.
//!
//! Every filesystem-touching step here is fallible on purpose; callers turn a
//! failure into `RuntimeHostError::RewindFailed` and surface it to the user.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::Utc;
use orca_core::config::file::RewindFileConfig;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::{Conversation, Message, is_internal_image_analysis};
use orca_core::cost_types::UsageTotals;
use orca_core::rewind::{
    ConversationRewindReport, RewindCheckpointView, RewindFilePreview, RewindFilesReport,
    RewindMode, RewindOutcome, RewindReport, RewindSkip, RewindStats, RewindWarning,
};
use orca_core::tool_types::{ToolName, ToolRequest};
use orca_provider::{ProviderConfig, context};
use orca_rewind::{
    Applier, BlobEncoding, BlobRef as RewindBlobRef, PlanOptions, PlanSkip, PlanWarning, PreImage,
    RestorePlan, RewindStore, SnapshotFileView, SnapshotView, build_plan, file_sha256,
};
use serde_json::Value;

use crate::session::InteractiveSession;
use crate::thread_store::{
    BlobRef, ConversationRewindRecord, FileSnapshotRecord, RewindFilesSummary, SessionRewindRecord,
    SessionWriter, SnapshotChangeStats, SnapshotFileEntry, read_effective_user_messages,
    read_session_meta, read_snapshots, read_transcript_before_message,
};
use crate::workspace_observation;

/// Set to a non-empty value other than `0` to disable file checkpointing.
const DISABLE_ENV: &str = "ORCA_DISABLE_FILE_HISTORY";
/// Any value (even empty) disables the turn-boundary workspace observation.
const DISABLE_OBSERVE_ENV: &str = "ORCA_REWIND_DISABLE_OBSERVE";
/// Characters of the first prompt line kept for the picker.
const PROMPT_PREVIEW_CHARS: usize = 80;
const REWIND_SCHEMA_VERSION: u32 = 1;
/// Retention window used by the crate-internal contract tests; the runtime
/// value comes from `[rewind] max_checkpoints` (see [`RewindSettings`]).
#[cfg(test)]
pub(crate) const MAX_CHECKPOINTS: usize = 100;
/// Dry-run preview caps: files and unified-diff bytes per file.
const MAX_PREVIEW_FILES: usize = 20;
const MAX_PREVIEW_DIFF_BYTES: usize = 64 * 1024;

/// The `[rewind]` section of the effective configuration with the environment
/// kill switches applied on top.
///
/// The environment always wins: `ORCA_DISABLE_FILE_HISTORY` overrides
/// `enabled` and `ORCA_REWIND_DISABLE_OBSERVE` overrides `observe_untracked`,
/// so an operator can turn capture off without editing a config file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RewindSettings {
    pub(crate) enabled: bool,
    pub(crate) observe_untracked: bool,
    pub(crate) max_checkpoints: usize,
}

impl Default for RewindSettings {
    fn default() -> Self {
        Self::from_parts(RewindFileConfig::default(), false, false)
    }
}

impl RewindSettings {
    /// Loads `[rewind]` from the effective configuration resolved at `cwd`.
    /// A configuration error falls back to the documented defaults instead of
    /// disabling checkpoints.
    pub(crate) fn load(cwd: &Path) -> Self {
        let config = orca_core::config::file::load_effective_config(
            cwd,
            orca_core::config::file::ConfigOverrides::default(),
        )
        .map(|config| config.rewind)
        .unwrap_or_default();
        Self::from_env(config)
    }

    fn from_env(config: RewindFileConfig) -> Self {
        let disable_file_history = std::env::var_os(DISABLE_ENV);
        let disable_observe = std::env::var_os(DISABLE_OBSERVE_ENV);
        Self::from_parts(
            config,
            env_disables_file_history(disable_file_history.as_deref()),
            disable_observe.is_some(),
        )
    }

    fn from_parts(
        config: RewindFileConfig,
        env_disable_file_history: bool,
        env_disable_observe: bool,
    ) -> Self {
        Self {
            enabled: config.enabled && !env_disable_file_history,
            observe_untracked: config.observe_untracked && !env_disable_observe,
            // A zero window would evict every checkpoint; one is the minimum
            // that still leaves a usable rewind target.
            max_checkpoints: usize::try_from(config.max_checkpoints)
                .unwrap_or(usize::MAX)
                .max(1),
        }
    }
}

/// Whether file checkpointing is on for the workspace at `cwd`.
pub(crate) fn is_rewind_enabled(cwd: &Path) -> bool {
    RewindSettings::load(cwd).enabled
}

/// `ORCA_DISABLE_FILE_HISTORY` uses the historical contract: an empty value or
/// `0` leaves capture on, anything else disables it.
fn env_disables_file_history(value: Option<&OsStr>) -> bool {
    match value {
        Some(value) => {
            let value = value.to_string_lossy();
            let value = value.trim();
            !(value.is_empty() || value == "0")
        }
        None => false,
    }
}

/// A user message that represents a real turn and can therefore be a rewind
/// target.
///
/// Pinned user messages count: headless sessions (`orca exec --save-history`)
/// submit their prompt with `PinnedUser` placement, and excluding them would
/// leave those sessions with no listable checkpoint. Pinned system context and
/// internal image-analysis records are still filtered out.
pub(crate) fn is_rewindable_user(message: &Message) -> bool {
    matches!(message,
        Message::User { content, images, .. }
            if !images.is_empty() || !is_internal_image_analysis(content))
}

// ---------------------------------------------------------------------------
// Per-turn capture
// ---------------------------------------------------------------------------

/// One captured file of a turn: its pre-image (possibly a tombstone) and the
/// digest observed after the most recent write.
struct CapturedFile {
    pre: PreImage,
    post_sha256: Option<String>,
}

struct RewindTurnState {
    root: PathBuf,
    session_id: String,
    store_dir: PathBuf,
    at_message_id: String,
    turn_id: Option<String>,
    ordinal: u32,
    /// Retention window from the `[rewind]` configuration at turn start.
    max_checkpoints: usize,
    start: Option<workspace_observation::Snapshot>,
    files: BTreeMap<String, CapturedFile>,
    limitations: BTreeSet<String>,
}

/// The per-turn checkpoint capture, shared through the thread extension store.
#[derive(Clone)]
pub(crate) struct RewindTurn {
    inner: Arc<Mutex<RewindTurnState>>,
}

fn lock_state(state: &Mutex<RewindTurnState>) -> MutexGuard<'_, RewindTurnState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts a turn's checkpoint capture, or `None` when checkpointing is off or
/// the session is not being recorded.
///
/// The writer and the conversation are passed as separate borrows because
/// [`InteractiveSession::runtime_parts`] already splits them; `turn_id` is the
/// runtime turn that owns the writes captured from now on.
pub(crate) fn begin_turn(
    config: &RunConfig,
    cwd: &Path,
    turn_id: &str,
    conversation: &Conversation,
    writer: Option<&mut SessionWriter>,
) -> Option<RewindTurn> {
    let settings = RewindSettings::load(cwd);
    if !settings.enabled || matches!(config.history_mode, HistoryMode::Disabled) {
        return None;
    }
    let writer = writer?;
    let session_id = writer.session_id()?;
    // The snapshot joins the transcript on the user message that is being
    // committed right now; without one the record could never be selected.
    let at_message_id = writer.last_committed_message_id()?;
    let root = cwd.canonicalize().ok()?;
    let store_dir = orca_core::home::orca_home()?
        .join("rewind")
        .join("sessions")
        .join(&session_id);
    let ordinal = conversation
        .messages
        .iter()
        .filter(|message| is_rewindable_user(message))
        .count()
        .saturating_sub(1) as u32;
    let start =
        settings
            .observe_untracked
            .then(|| workspace_observation::Snapshot::capture(&root));
    Some(RewindTurn {
        inner: Arc::new(Mutex::new(RewindTurnState {
            root,
            session_id,
            store_dir,
            at_message_id,
            turn_id: Some(turn_id.to_string()),
            ordinal,
            max_checkpoints: settings.max_checkpoints,
            start,
            files: BTreeMap::new(),
            limitations: BTreeSet::new(),
        })),
    })
}

impl RewindTurn {
    /// Captures the pre-image of a write-class tool's target, if the target is
    /// a regular file inside the workspace root. Called before the tool runs.
    ///
    /// Errors never propagate: they are recorded as snapshot limitations so
    /// the tool keeps executing and the checkpoint is honest about its gaps.
    pub(crate) fn capture_pre_image(&self, cwd: &Path, request: &ToolRequest) {
        if !matches!(request.name, ToolName::Edit | ToolName::WriteFile) {
            return;
        }
        let Some(raw_arguments) = request.raw_arguments.as_deref() else {
            return;
        };
        let Ok(arguments) = serde_json::from_str::<Value>(raw_arguments) else {
            return;
        };
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return;
        };
        let mut state = lock_state(&self.inner);
        let resolved = match resolve_request_path(&state.root, cwd, path) {
            Ok(resolved) => resolved,
            Err(limitation) => {
                state.limitations.insert(limitation);
                return;
            }
        };
        let (workspace_relative, absolute) = resolved;
        // Same turn, same file: keep the original pre-image and only refresh
        // the post digest (the content written by the previous tool call).
        if let Some(captured) = state.files.get_mut(&workspace_relative) {
            if let Ok(digest) = file_sha256(&absolute) {
                captured.post_sha256 = digest;
            }
            return;
        }
        let store = match RewindStore::open(&state.store_dir) {
            Ok(store) => store,
            Err(error) => {
                state.limitations.insert(format!("io:{error}"));
                return;
            }
        };
        match store.put_pre_image(&absolute) {
            Ok(pre) => {
                state.files.insert(
                    workspace_relative,
                    CapturedFile {
                        pre,
                        post_sha256: None,
                    },
                );
            }
            Err(error) => {
                state.limitations.insert(capture_limitation(&error));
            }
        }
    }

    /// Ends the turn: refreshes post digests, compares the bounded workspace
    /// observation with the turn start, and durably appends the
    /// `session.snapshot` record when at least one file was captured.
    pub(crate) fn finish(
        &self,
        session: &mut InteractiveSession,
    ) -> io::Result<Option<FileSnapshotRecord>> {
        let Some(record) = self.finish_record()? else {
            return Ok(None);
        };
        let writer = session.writer_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "session writer is unavailable; the file checkpoint was not saved",
            )
        })?;
        writer.append_file_snapshot(record.clone())?;
        // Retention is best effort: losing the watermark only means the
        // picker keeps offering older checkpoints that still exist on disk.
        if let Err(error) =
            enforce_snapshot_retention_with_limit(writer, &record.session_id, self.max_checkpoints())
        {
            tracing::warn!(%error, "rewind snapshot retention watermark was not saved");
        }
        Ok(Some(record))
    }

    /// Builds the durable snapshot record without appending it. Exposed for
    /// the crate-internal capture tests; [`Self::finish`] is the runtime path.
    pub(crate) fn finish_record(&self) -> io::Result<Option<FileSnapshotRecord>> {
        let record = {
            let mut state = lock_state(&self.inner);
            let root = state.root.clone();
            for (path, captured) in state.files.iter_mut() {
                if let Ok(digest) = file_sha256(&root.join(path)) {
                    captured.post_sha256 = digest;
                }
            }
            let mut limitations = std::mem::take(&mut state.limitations);
            let mut untracked = Vec::new();
            if let Some(start) = state.start.take() {
                let end = workspace_observation::Snapshot::capture(&root);
                let diff = start.compare(&end);
                for limitation in diff
                    .get("limitations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    limitations.insert(limitation.to_string());
                }
                for change in diff
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(path) = change.get("path").and_then(Value::as_str) else {
                        continue;
                    };
                    let observed_after = change.get("after_sha256").and_then(Value::as_str);
                    match state.files.get(path) {
                        // Never touched by a write tool: bash/external change.
                        None => untracked.push(path.to_string()),
                        // Touched, but the disk no longer holds what the agent
                        // wrote last: also outside the restore guarantee.
                        Some(captured) if captured.post_sha256.as_deref() != observed_after => {
                            untracked.push(path.to_string());
                        }
                        Some(_) => {}
                    }
                }
            }
            if state.files.is_empty() {
                None
            } else {
                let change_stats = change_stats_for(&state.files, &root, &state.store_dir);
                let files = state
                    .files
                    .iter()
                    .map(|(path, captured)| {
                        (
                            path.clone(),
                            SnapshotFileEntry {
                                pre: stored_blob_ref(&captured.pre),
                                post_sha256: captured.post_sha256.clone(),
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                Some(FileSnapshotRecord {
                    schema_version: REWIND_SCHEMA_VERSION,
                    snapshot_id: format!("snap_{}", uuid::Uuid::now_v7()),
                    session_id: state.session_id.clone(),
                    at_message_id: state.at_message_id.clone(),
                    turn_id: state.turn_id.clone(),
                    ordinal: state.ordinal,
                    root: root.to_string_lossy().into_owned(),
                    workspace_fingerprint: workspace_fingerprint(&root),
                    complete: limitations.is_empty(),
                    start_digest: None,
                    end_digest: None,
                    files,
                    change_stats: Some(change_stats),
                    untracked,
                    limitations: limitations.into_iter().collect(),
                    recorded_at: Utc::now(),
                })
            }
        };
        Ok(record)
    }

    /// Retention window captured when the turn started.
    fn max_checkpoints(&self) -> usize {
        lock_state(&self.inner).max_checkpoints
    }
}

/// Resolves a tool argument path against the turn's `cwd` and the checkpoint
/// root. Returns `(workspace_relative, absolute)` or a snapshot limitation.
///
/// Paths outside the root, absolute paths, `..` components, symlinks and
/// non-regular files are refused; they are recorded as limitations instead of
/// being captured.
fn resolve_request_path(root: &Path, cwd: &Path, raw: &str) -> Result<(String, PathBuf), String> {
    let raw_path = Path::new(raw);
    if raw_path.as_os_str().is_empty() {
        return Err("outside_root".to_string());
    }
    let mut relative = PathBuf::new();
    for component in raw_path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            // "./x" names the same file as "x"; just drop the prefix.
            Component::CurDir => {}
            // Absolute paths, `..` and Windows prefixes can escape the root.
            _ => return Err("outside_root".to_string()),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err("outside_root".to_string());
    }
    let relative_from_root = if cwd == root {
        relative
    } else {
        let cwd_relative = cwd
            .strip_prefix(root)
            .map_err(|_| "outside_root".to_string())?;
        cwd_relative.join(relative)
    };
    let mut workspace_relative = String::new();
    let components = relative_from_root
        .components()
        .collect::<Vec<Component<'_>>>();
    for component in &components {
        let Component::Normal(part) = component else {
            return Err("outside_root".to_string());
        };
        let Some(part) = part.to_str() else {
            return Err("non_utf8_path".to_string());
        };
        if part.is_empty() {
            return Err("outside_root".to_string());
        }
        if !workspace_relative.is_empty() {
            workspace_relative.push('/');
        }
        workspace_relative.push_str(part);
    }
    // Reject symlinked components (a symlink inside the workspace could read
    // or later write outside it) and non-regular final targets.
    let mut current = root.to_path_buf();
    let last_index = components.len().saturating_sub(1);
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            unreachable!("components were normalized above");
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err("symlink".to_string());
                }
                if index == last_index && !metadata.file_type().is_file() {
                    return Err("not_regular".to_string());
                }
                if index != last_index && !metadata.file_type().is_dir() {
                    return Err("outside_root".to_string());
                }
            }
            // A new file below an already verified directory is a tombstone in
            // the parent's history, which is exactly what we want to record.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("io:{error}")),
        }
    }
    let absolute = root.join(&workspace_relative);
    Ok((workspace_relative, absolute))
}

fn capture_limitation(error: &io::Error) -> String {
    match orca_rewind::rewind_error(error) {
        Some(orca_rewind::RewindError::TooLarge { .. }) => "too_large".to_string(),
        Some(orca_rewind::RewindError::NotRegular { .. }) => "not_regular".to_string(),
        Some(orca_rewind::RewindError::PathEscape { .. })
        | Some(orca_rewind::RewindError::RootNotDirectory { .. }) => "outside_root".to_string(),
        _ => format!("io:{error}"),
    }
}

#[cfg(unix)]
fn workspace_fingerprint(root: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(root)
        .ok()
        .map(|metadata| format!("dev:{},ino:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn workspace_fingerprint(_root: &Path) -> Option<String> {
    None
}

/// Line statistics for the turn's captured files: pre-image -> current
/// content. Unreadable, oversized or non-UTF-8 inputs set `omitted` without
/// discarding the counts already accumulated.
fn change_stats_for(
    files: &BTreeMap<String, CapturedFile>,
    root: &Path,
    store_dir: &Path,
) -> SnapshotChangeStats {
    let store = RewindStore::open(store_dir).ok();
    let mut insertions = 0_u64;
    let mut deletions = 0_u64;
    let mut omitted = store.is_none();
    for (path, captured) in files {
        let old = match &captured.pre {
            PreImage::Missing => Vec::new(),
            PreImage::Blob(blob) => match store.as_ref().map(|store| store.read_blob(blob)) {
                Some(Ok(bytes)) => bytes,
                _ => {
                    omitted = true;
                    continue;
                }
            },
        };
        let current = match read_current_source(&root.join(path)) {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(_) => {
                omitted = true;
                continue;
            }
        };
        match orca_rewind::line_diff_stats(&old, &current) {
            Some(stats) => {
                insertions = insertions.saturating_add(stats.insertions);
                deletions = deletions.saturating_add(stats.deletions);
            }
            None => omitted = true,
        }
    }
    SnapshotChangeStats {
        files: u32::try_from(files.len()).unwrap_or(u32::MAX),
        insertions,
        deletions,
        omitted,
    }
}

/// Current bytes of a captured path for statistics. A missing file is the
/// empty side of the diff (every line of the pre-image counts as deleted); a
/// non-regular or oversized file cannot be diffed.
fn read_current_source(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Some(Vec::new())),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > orca_rewind::MAX_SNAPSHOT_FILE_BYTES {
        return Ok(None);
    }
    std::fs::read(path).map(Some)
}

fn stored_blob_ref(pre: &PreImage) -> Option<BlobRef> {
    match pre {
        PreImage::Missing => None,
        PreImage::Blob(blob) => Some(BlobRef {
            sha256: blob.sha256.clone(),
            size: blob.size,
            mode: blob.mode,
            encoding: match blob.encoding {
                BlobEncoding::Raw => "raw".to_string(),
                BlobEncoding::Zstd => "zstd".to_string(),
            },
        }),
    }
}

fn pre_image(blob: Option<&BlobRef>) -> PreImage {
    match blob {
        None => PreImage::Missing,
        Some(blob) => PreImage::Blob(RewindBlobRef {
            sha256: blob.sha256.clone(),
            size: blob.size,
            mode: blob.mode,
            encoding: match blob.encoding.as_str() {
                "zstd" => BlobEncoding::Zstd,
                _ => BlobEncoding::Raw,
            },
        }),
    }
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// Lists the rewindable user messages of the effective conversation together
/// with the file statistics of their checkpoint.
pub(crate) fn list_checkpoints(
    session: &InteractiveSession,
) -> io::Result<Vec<RewindCheckpointView>> {
    let Some(path) = session.writer_path() else {
        return Ok(Vec::new());
    };
    // A disabled `[rewind]` section must look like a session with no
    // checkpoints rather than exposing the ones already on disk.
    if !is_rewind_enabled(&session_workspace(path)?) {
        return Ok(Vec::new());
    }
    let mut views = checkpoint_views(path)?;
    // An unfinished journal means a previous file restore did not commit; the
    // workspace is not a trustworthy target until it is resolved.
    if let Some(op_id) = pending_rewind_journal(session.transcript_session_id().as_deref())? {
        for view in &mut views {
            view.files_available = false;
            view.unavailable_reasons
                .push(format!("pending_rewind_recovery: {op_id}"));
        }
    }
    Ok(views)
}

/// Workspace a saved session was recorded in; used to resolve its effective
/// `[rewind]` configuration.
fn session_workspace(path: &Path) -> io::Result<PathBuf> {
    read_session_meta(path).map(|meta| PathBuf::from(meta.cwd))
}

/// Path-only form of [`list_checkpoints`], reused by the contract tests.
pub(crate) fn checkpoint_views(path: &Path) -> io::Result<Vec<RewindCheckpointView>> {
    let users = read_effective_user_messages(path)?;
    let snapshots = read_snapshots(path)?;
    let evicted_through = snapshot_retention_watermark(path)?;
    Ok(users
        .into_iter()
        .map(|user| {
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.at_message_id == user.message_id);
            let stats = snapshot.and_then(|snapshot| snapshot.change_stats.as_ref());
            let evicted = snapshot.is_some()
                && evicted_through
                    .is_some_and(|through| snapshot.is_some_and(|s| s.ordinal <= through));
            let mut unavailable_reasons = Vec::new();
            if snapshot.is_none() {
                unavailable_reasons.push("snapshot_missing".to_string());
            } else if evicted {
                unavailable_reasons.push("snapshot_evicted".to_string());
            }
            let has_snapshot = snapshot.is_some();
            RewindCheckpointView {
                message_id: user.message_id,
                ordinal: user.ordinal,
                prompt_preview: prompt_preview(&user.prompt),
                recorded_at: snapshot.map(|snapshot| snapshot.recorded_at),
                files_changed: stats.map_or(0, |stats| u64::from(stats.files)),
                insertions: stats.map_or(0, |stats| stats.insertions),
                deletions: stats.map_or(0, |stats| stats.deletions),
                has_snapshot,
                snapshot_evicted: evicted,
                conversation_available: true,
                files_available: has_snapshot && !evicted,
                unavailable_reasons,
            }
        })
        .collect())
}

fn prompt_preview(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or_default().trim();
    let mut preview = first_line
        .chars()
        .take(PROMPT_PREVIEW_CHARS)
        .collect::<String>();
    if first_line.chars().count() > PROMPT_PREVIEW_CHARS {
        preview.push('…');
    }
    preview
}

/// Summary text and aggregated usage produced for a rewind range.
pub(crate) struct SummarizedRange {
    pub(crate) summary: String,
    pub(crate) usage: Option<UsageTotals>,
}

/// Summarizes `messages` into one historical context summary with the
/// auxiliary model the session already uses for compaction.
///
/// This deliberately does not go through [`context::compact_with_summary`]:
/// that path only requests a summary once the prompt exceeds the context
/// window, which would make a rewind of a short range fail. `summarize_range`
/// always asks the model.
pub(crate) fn summarize_messages(
    config: &RunConfig,
    cwd: &Path,
    messages: &[Message],
) -> io::Result<SummarizedRange> {
    // Reserved for the `pre_rewind`/`post_rewind` hooks of a later milestone;
    // summarizing itself needs no working directory today.
    let _ = cwd;
    if messages.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nothing to summarize in the selected range",
        ));
    }
    let model = config.model.as_option();
    let context_config = context::ContextConfig::for_model_with_runtime(
        model.as_deref(),
        &config.model_runtime,
        config.reasoning_effort,
    );
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::auxiliary_model().to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
        max_output_tokens: None,
    };
    let summary = orca_provider::summarize_range(
        config.provider,
        messages,
        &context_config,
        &provider_config,
        None,
    )
    .ok_or_else(|| io::Error::other("the model returned no summary for the selected range"))?;
    let usage = summary.usage.map(|usage| {
        let mut tracker = crate::cost::CostTracker::new(Some(orca_core::model::auxiliary_model()));
        tracker.add_usage(usage)
    });
    Ok(SummarizedRange {
        summary: summary.text,
        usage,
    })
}

fn current_system_prompt(session: &InteractiveSession) -> String {
    session
        .conversation()
        .messages
        .iter()
        .find_map(|message| match message {
            Message::System { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn rewind_cwd(config: &RunConfig) -> PathBuf {
    config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Validates and (unless `dry_run`) applies a rewind to `target_message_id`.
///
/// Order matters and mirrors the crash story in the design doc: plan, apply
/// files, rewrite the in-memory conversation, then durably append the
/// `session.rewind` record. A failure to append is reported but never rolls
/// the already-applied files back (that would need a second write).
pub(crate) fn apply_rewind(
    session: &mut InteractiveSession,
    config: &RunConfig,
    target_message_id: &str,
    mode: RewindMode,
    dry_run: bool,
) -> io::Result<RewindOutcome> {
    let (transcript_path, writer_session_id) = {
        let writer = session.writer_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "rewind requires a recorded session; history is disabled or unavailable",
            )
        })?;
        (writer.path().to_path_buf(), writer.session_id())
    };
    let users = read_effective_user_messages(&transcript_path)?;
    let snapshots = read_snapshots(&transcript_path)?;
    let target_user = users
        .iter()
        .find(|user| user.message_id == target_message_id);
    let target_snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.at_message_id == target_message_id);
    if mode.includes_conversation() && target_user.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved message matches '{target_message_id}'"),
        ));
    }
    if mode.includes_files() && target_snapshot.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no file checkpoint matches '{target_message_id}'"),
        ));
    }
    let target_ordinal = target_snapshot
        .map(|snapshot| snapshot.ordinal)
        .or_else(|| target_user.map(|user| user.ordinal))
        .unwrap_or_default();
    let mut report = RewindReport {
        target_message_id: target_message_id.to_string(),
        ordinal: target_ordinal,
        mode: mode.as_str().to_string(),
        dry_run,
        ..RewindReport::default()
    };
    let mut warnings: Vec<RewindWarning> = Vec::new();

    // A journal left by an interrupted apply means the workspace state is
    // uncertain: surface it, and refuse a new file restore until it is
    // resolved. Conversation-only rewinds stay available.
    if let Some(op_id) = pending_rewind_journal(writer_session_id.as_deref())? {
        warnings.push(RewindWarning {
            kind: "pending_journal".to_string(),
            paths: Vec::new(),
            detail: Some(format!(
                "an unfinished rewind operation ({op_id}) must be resolved first; run                  `orca rewind --rollback {op_id}` or remove its journal"
            )),
        });
        if !dry_run && mode.includes_files() {
            return Err(io::Error::other(format!(
                "an unfinished rewind operation ({op_id}) is pending; resolve it with                  `orca rewind --rollback {op_id}` before restoring files"
            )));
        }
    }

    let mut applied_files: Option<RewindFilesSummary> = None;
    let mut op_id: Option<String> = None;
    if mode.includes_files() {
        let snapshot = target_snapshot.expect("validated above");
        let snapshot_root = PathBuf::from(&snapshot.root);
        let root = snapshot_root.canonicalize().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "snapshot root '{}' is not accessible: {error}",
                    snapshot_root.display()
                ),
            )
        })?;
        // The workspace this session was recorded in must still be the
        // workspace of the checkpoint: restoring pre-images into a resumed
        // session that moved to another directory would write into the wrong
        // tree. Comparing canonical paths also normalizes symlinked roots.
        let session_meta = read_session_meta(&transcript_path)?;
        let session_root = PathBuf::from(&session_meta.cwd)
            .canonicalize()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "session workspace '{}' is not accessible: {error}",
                        session_meta.cwd
                    ),
                )
            })?;
        if session_root != root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "session workspace '{}' does not match the checkpoint workspace \
                     '{}'; refusing to restore files into a different workspace",
                    session_root.display(),
                    root.display()
                ),
            ));
        }
        // A restore never remaps paths across roots: every snapshot that
        // participates in the plan must have been recorded in this workspace.
        for candidate in snapshots
            .iter()
            .filter(|candidate| candidate.ordinal >= target_ordinal)
        {
            let candidate_root = PathBuf::from(&candidate.root);
            let canonical = candidate_root.canonicalize().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "snapshot root '{}' is not accessible: {error}",
                        candidate_root.display()
                    ),
                )
            })?;
            if canonical != root {
                warnings.push(RewindWarning {
                    kind: "root_mismatch".to_string(),
                    paths: Vec::new(),
                    detail: Some(format!(
                        "snapshot root '{}' does not match workspace root '{}'",
                        candidate_root.display(),
                        root.display()
                    )),
                });
                return Err(io::Error::other(format!(
                    "snapshot root '{}' does not match workspace root '{}'",
                    candidate_root.display(),
                    root.display()
                )));
            }
        }
        let store_session_id = if snapshot.session_id.is_empty() {
            writer_session_id.clone().unwrap_or_default()
        } else {
            snapshot.session_id.clone()
        };
        let store_dir = orca_core::home::orca_home()
            .ok_or_else(|| io::Error::other("the Orca home directory is unavailable"))?
            .join("rewind")
            .join("sessions")
            .join(&store_session_id);
        let store = RewindStore::open(&store_dir)?;
        let views = snapshot_views(&snapshots);
        let plan = build_plan(&root, &store, &views, PlanOptions { target_ordinal })?;
        report.files = files_report_from_plan(&plan);
        report.stats = RewindStats {
            insertions: plan.insertions,
            deletions: plan.deletions,
            omitted: plan.stats_omitted,
        };
        for warning in &plan.warnings {
            warnings.push(stored_warning(warning));
        }
        if dry_run {
            // Previews are best effort: a preview failure must not hide the
            // plan the user needs to confirm.
            match orca_rewind::plan_diff_previews(
                &root,
                &store,
                &plan,
                MAX_PREVIEW_FILES,
                MAX_PREVIEW_DIFF_BYTES,
            ) {
                Ok(previews) => {
                    report.previews = previews.into_iter().map(stored_preview).collect();
                }
                Err(error) => warnings.push(RewindWarning {
                    kind: "preview_unavailable".to_string(),
                    paths: Vec::new(),
                    detail: Some(error.to_string()),
                }),
            }
        } else {
            let applier = Applier::new(root, store, store_dir.join("journals"));
            let apply_report = applier.apply(&plan)?;
            op_id = Some(apply_report.op_id.clone());
            report.files = RewindFilesReport {
                changed: apply_report.applied,
                deleted: apply_report.deleted,
                unchanged: plan.unchanged,
                skipped: apply_report.skipped.iter().map(stored_skip).collect(),
            };
            if apply_report.partial {
                warnings.push(RewindWarning {
                    kind: "partial_apply".to_string(),
                    paths: Vec::new(),
                    detail: Some(
                        "the file restore stopped early; a journal is retained so the \
                         operation can be rolled back"
                            .to_string(),
                    ),
                });
            }
            applied_files = Some(RewindFilesSummary {
                restored: u32::try_from(apply_report.applied).unwrap_or(u32::MAX),
                deleted: u32::try_from(apply_report.deleted).unwrap_or(u32::MAX),
                skipped: u32::try_from(report.files.skipped.len()).unwrap_or(u32::MAX),
            });
        }
    }

    let mut conversation_report: Option<ConversationRewindReport> = None;
    let mut conversation_record: Option<ConversationRewindRecord> = None;
    let mut prompt: Option<String> = None;
    let mut user_ordinal = target_ordinal;
    let mut kept_messages = 0_u32;
    let mut kept_compactions = 0_u32;
    let mut kept_summaries = 0_u32;
    let mut dropped_messages = 0_u32;

    if mode.is_summarize() {
        let split = crate::thread_store::read_transcript_split_at_message(
            &transcript_path,
            target_message_id,
        )?;
        user_ordinal = split.ordinal;
        prompt = Some(split.prompt.clone());
        let kept_from_before = u32::try_from(split.before.messages.len()).unwrap_or(u32::MAX);
        let kept_after = split.kept_after_messages;
        let kind = match mode {
            RewindMode::SummarizeFrom => "summarize_from",
            _ => "summarize_up_to",
        };
        let kept = match mode {
            RewindMode::SummarizeFrom => kept_from_before,
            _ => kept_after,
        };
        conversation_report = Some(ConversationRewindReport {
            kind: kind.to_string(),
            kept_messages: kept,
        });
        if dry_run {
            // Dry-run must not spend tokens: only describe what apply would do.
            warnings.push(RewindWarning {
                kind: "summarize_requires_model".to_string(),
                paths: Vec::new(),
                detail: Some("apply will call the summarizer".to_string()),
            });
        } else {
            let system_prompt = current_system_prompt(session);
            let range = match mode {
                RewindMode::SummarizeFrom => {
                    summarize_messages(config, &rewind_cwd(config), &split.after_messages)?
                }
                _ => summarize_messages(config, &rewind_cwd(config), &split.before.messages)?,
            };
            match mode {
                RewindMode::SummarizeFrom => {
                    kept_messages = kept_from_before;
                    kept_compactions =
                        u32::try_from(split.before.compactions.len()).unwrap_or(u32::MAX);
                    kept_summaries =
                        u32::try_from(split.before.summaries.len()).unwrap_or(u32::MAX);
                    let mut conversation =
                        crate::history::resume_conversation(&split.before, system_prompt);
                    conversation
                        .messages
                        .push(Message::pinned_system(range.summary.clone()));
                    session.replace_conversation(conversation);
                    dropped_messages = 0;
                }
                _ => {
                    kept_messages = kept_after;
                    kept_compactions = 0;
                    kept_summaries = 0;
                    dropped_messages = split.dropped_messages;
                    let mut conversation = Conversation::new();
                    conversation.add_system(system_prompt);
                    conversation
                        .messages
                        .push(Message::pinned_system(range.summary.clone()));
                    conversation
                        .messages
                        .extend(split.after_messages.iter().cloned());
                    session.replace_conversation(conversation);
                }
            }
            conversation_record = Some(ConversationRewindRecord {
                kind: kind.to_string(),
                summary: Some(range.summary),
                summary_usage: range.usage,
            });
        }
    } else if mode.includes_conversation() {
        let cut = read_transcript_before_message(&transcript_path, target_message_id)?;
        user_ordinal = cut.ordinal;
        prompt = Some(cut.prompt.clone());
        kept_messages = cut.kept_messages;
        kept_compactions = cut.kept_compactions;
        kept_summaries = cut.kept_summaries;
        if !dry_run {
            // Rebuild the conversation from the truncated transcript so the
            // system prompt and compaction replay match a fresh resume.
            let system_prompt = current_system_prompt(session);
            let conversation = crate::history::resume_conversation(&cut.transcript, system_prompt);
            session.replace_conversation(conversation);
            if cut.kept_summaries == 0 {
                session.conversation_mut().clear_summary_state();
            }
        }
        conversation_report = Some(ConversationRewindReport {
            kind: "truncate".to_string(),
            kept_messages: cut.kept_messages,
        });
        conversation_record = Some(ConversationRewindRecord {
            kind: "truncate".to_string(),
            summary: None,
            summary_usage: None,
        });
    }

    let rewind_id_value = format!("rw_{}", uuid::Uuid::now_v7());
    if !dry_run {
        let record = SessionRewindRecord {
            schema_version: REWIND_SCHEMA_VERSION,
            session_id: writer_session_id.unwrap_or_default(),
            rewind_id: rewind_id_value.clone(),
            at_message_id: target_message_id.to_string(),
            kept_messages,
            kept_compactions,
            kept_summaries,
            dropped_messages,
            conversation: conversation_record,
            files: applied_files,
            recorded_at: Utc::now(),
        };
        let writer = session.writer_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "session writer is unavailable; the rewind record was not saved",
            )
        })?;
        writer.append_session_rewind(record).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "the workspace was rewound but the session.rewind record could not be \
                     written: {error}"
                ),
            )
        })?;
    }

    report.warnings = warnings;
    report.conversation = conversation_report;
    report.op_id = op_id;
    report.rewind_id = (!dry_run).then_some(rewind_id_value);
    Ok(RewindOutcome {
        report,
        prompt,
        user_ordinal,
    })
}

/// Rewind blob store directory for a session.
fn session_rewind_store_dir(session_id: Option<&str>) -> io::Result<PathBuf> {
    Ok(orca_core::home::orca_home()
        .ok_or_else(|| io::Error::other("the Orca home directory is unavailable"))?
        .join("rewind")
        .join("sessions")
        .join(session_id.unwrap_or_default()))
}

/// Operation id of a journal left behind by an interrupted file restore.
fn pending_rewind_journal(session_id: Option<&str>) -> io::Result<Option<String>> {
    let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) else {
        return Ok(None);
    };
    let store_dir = session_rewind_store_dir(Some(session_id))?;
    let journal_dir = store_dir.join("journals");
    // Reading the checkpoint list must not create store directories.
    if !journal_dir.exists() {
        return Ok(None);
    }
    let store = RewindStore::open(&store_dir)?;
    Applier::new(PathBuf::new(), store, journal_dir).pending_journal()
}

/// Highest ordinal whose snapshot blob state is no longer promised.
fn snapshot_retention_watermark(path: &Path) -> io::Result<Option<u32>> {
    Ok(crate::thread_store::read_snapshot_retention(path)?
        .map(|record| record.evicted_through_ordinal))
}

/// Retains the newest [`MAX_CHECKPOINTS`] snapshots by turn ordinal and
/// appends an eviction watermark for the rest; then best-effort garbage
/// collects the blobs that only evicted snapshots referenced.
#[cfg(test)]
pub(crate) fn enforce_snapshot_retention(
    writer: &mut SessionWriter,
    session_id: &str,
) -> io::Result<()> {
    enforce_snapshot_retention_with_limit(writer, session_id, MAX_CHECKPOINTS)
}

/// [`enforce_snapshot_retention`] for the `[rewind] max_checkpoints` window
/// (clamped to at least one). The watermark is audit state: the
/// `session.snapshot` records themselves stay in the transcript.
pub(crate) fn enforce_snapshot_retention_with_limit(
    writer: &mut SessionWriter,
    session_id: &str,
    max_checkpoints: usize,
) -> io::Result<()> {
    let max_checkpoints = max_checkpoints.max(1);
    let path = writer.path().to_path_buf();
    let mut snapshots = read_snapshots(&path)?;
    if snapshots.len() <= max_checkpoints {
        return Ok(());
    }
    snapshots.sort_by_key(|snapshot| snapshot.ordinal);
    let oldest_kept = snapshots[snapshots.len() - max_checkpoints].ordinal;
    let evicted_through_ordinal = oldest_kept.saturating_sub(1);
    if snapshot_retention_watermark(&path)?
        .is_some_and(|previous| previous >= evicted_through_ordinal)
    {
        return Ok(());
    }
    writer.append_snapshot_retention(crate::thread_store::SnapshotRetentionRecord {
        schema_version: REWIND_SCHEMA_VERSION,
        session_id: session_id.to_string(),
        evicted_through_ordinal,
        recorded_at: Utc::now(),
    })?;
    // Eviction promises only conversation rewind for the dropped ordinals, so
    // their pre-images are no longer needed. Failure to collect is logged, not
    // propagated: retention itself already committed.
    if let Err(error) = gc_evicted_snapshot_blobs(&path, session_id) {
        tracing::warn!(%error, "rewind checkpoint blobs were not garbage-collected");
    }
    Ok(())
}

/// Deletes the blobs that only evicted snapshots (`ordinal <=` the retention
/// watermark) referenced. Retained snapshots and their pre-images stay live.
fn gc_evicted_snapshot_blobs(path: &Path, session_id: &str) -> io::Result<()> {
    let Some(home) = orca_core::home::orca_home() else {
        return Ok(());
    };
    let store_dir = home.join("rewind").join("sessions").join(session_id);
    if !store_dir.is_dir() {
        return Ok(());
    }
    let watermark = snapshot_retention_watermark(path)?;
    let mut live = BTreeSet::new();
    for snapshot in read_snapshots(path)? {
        if watermark.is_some_and(|through| snapshot.ordinal <= through) {
            continue;
        }
        for entry in snapshot.files.values() {
            if let Some(blob) = entry.pre.as_ref() {
                live.insert(blob.sha256.clone());
            }
        }
    }
    let store = RewindStore::open(&store_dir)?;
    let report = store.gc(&live)?;
    if report.removed_blobs > 0 {
        tracing::debug!(
            removed_blobs = report.removed_blobs,
            removed_bytes = report.removed_bytes,
            "rewind checkpoint blobs garbage-collected"
        );
    }
    Ok(())
}

fn snapshot_views(snapshots: &[FileSnapshotRecord]) -> Vec<SnapshotView> {
    snapshots
        .iter()
        .map(|snapshot| SnapshotView {
            ordinal: snapshot.ordinal,
            at_message_id: snapshot.at_message_id.clone(),
            files: snapshot
                .files
                .iter()
                .map(|(path, entry)| {
                    (
                        path.clone(),
                        SnapshotFileView {
                            pre: pre_image(entry.pre.as_ref()),
                            post_sha256: entry.post_sha256.clone(),
                        },
                    )
                })
                .collect(),
        })
        .collect()
}

fn files_report_from_plan(plan: &RestorePlan) -> RewindFilesReport {
    RewindFilesReport {
        changed: plan.writes.len() as u64,
        deleted: plan.deletes.len() as u64,
        unchanged: plan.unchanged,
        skipped: plan.skipped.iter().map(stored_skip).collect(),
    }
}

fn stored_skip(skip: &PlanSkip) -> RewindSkip {
    RewindSkip {
        path: skip.path.clone(),
        reason: skip.reason.clone(),
    }
}

fn stored_preview(preview: orca_rewind::FileDiffPreview) -> RewindFilePreview {
    RewindFilePreview {
        path: preview.path,
        diff: preview.diff,
        truncated: preview.truncated,
        binary: preview.binary,
        insertions: preview.insertions,
        deletions: preview.deletions,
    }
}

fn stored_warning(warning: &PlanWarning) -> RewindWarning {
    RewindWarning {
        kind: warning.kind.clone(),
        paths: warning.paths.clone(),
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::conversation::{ImageDetail, ImageInput, ImageSource};

    #[test]
    fn rewindable_user_includes_pinned_turns_but_not_internal_analysis() {
        // Pinned user prompts are real turns: headless sessions submit their
        // prompt with pinned placement and must stay rewindable.
        let mut conversation = Conversation::new();
        conversation.add_user("first".to_string());
        conversation.add_user_pinned("pinned context".to_string());
        assert_eq!(
            conversation
                .messages
                .iter()
                .filter(|message| is_rewindable_user(message))
                .count(),
            2
        );

        // Internal image-analysis sidecar records are not turns.
        let mut conversation = Conversation::new();
        conversation.add_user("human".to_string());
        conversation.add_user(format!(
            "[Image analysis:{}]\n{}",
            "a".repeat(64),
            serde_json::json!({
                "trust": "untrusted_observation_data",
                "description": "saved image",
                "query": "human",
                "image_count": 1,
                "provenance": null,
            })
        ));
        assert_eq!(
            conversation
                .messages
                .iter()
                .filter(|message| is_rewindable_user(message))
                .count(),
            1
        );

        // A user message with images is a turn even without plain text.
        let mut conversation = Conversation::new();
        conversation.add_user_with_images(
            "image turn".to_string(),
            vec![ImageInput {
                source: ImageSource::Url {
                    url: "https://example.com/image.png".into(),
                },
                detail: ImageDetail::Auto,
            }],
        );
        assert!(conversation.messages.iter().all(is_rewindable_user));
    }

    #[test]
    fn request_paths_outside_the_root_are_limitations() {
        let root = tempfile::tempdir().expect("root");
        let root = root.path().canonicalize().expect("canonical root");
        let cwd = root.clone();
        assert_eq!(
            resolve_request_path(&root, &cwd, "../escape.rs"),
            Err("outside_root".to_string())
        );
        assert_eq!(
            resolve_request_path(&root, &cwd, "/abs/escape.rs"),
            Err("outside_root".to_string())
        );
        let (relative, absolute) =
            resolve_request_path(&root, &cwd, "src/new.rs").expect("inside path");
        assert_eq!(relative, "src/new.rs");
        assert_eq!(absolute, root.join("src/new.rs"));
        let (relative, _) =
            resolve_request_path(&root, &cwd, "./src/new.rs").expect("dot prefix is the same path");
        assert_eq!(relative, "src/new.rs");
    }

    #[test]
    fn rewind_settings_default_to_capturing_with_the_default_window() {
        let settings = RewindSettings::default();
        assert!(settings.enabled);
        assert!(settings.observe_untracked);
        assert_eq!(settings.max_checkpoints, MAX_CHECKPOINTS);
    }

    #[test]
    fn env_kill_switches_override_the_file_configuration() {
        // `ORCA_DISABLE_FILE_HISTORY` keeps the historical contract: empty and
        // `0` mean "not disabled", anything else disables.
        assert!(!env_disables_file_history(None));
        assert!(!env_disables_file_history(Some(OsStr::new(""))));
        assert!(!env_disables_file_history(Some(OsStr::new("0"))));
        assert!(env_disables_file_history(Some(OsStr::new("1"))));

        // A set environment kill switch disables what the file enables.
        let settings = RewindSettings::from_parts(RewindFileConfig::default(), true, true);
        assert!(!settings.enabled);
        assert!(!settings.observe_untracked);

        // The environment can never re-enable a disabled file.
        let settings = RewindSettings::from_parts(
            RewindFileConfig {
                enabled: false,
                observe_untracked: false,
                max_checkpoints: 5,
            },
            false,
            false,
        );
        assert!(!settings.enabled);
        assert!(!settings.observe_untracked);
        assert_eq!(settings.max_checkpoints, 5);

        // A zero retention window is clamped to the one checkpoint that still
        // leaves a rewind target.
        let settings = RewindSettings::from_parts(
            RewindFileConfig {
                max_checkpoints: 0,
                ..RewindFileConfig::default()
            },
            false,
            false,
        );
        assert_eq!(settings.max_checkpoints, 1);
    }

    #[test]
    fn rewind_settings_load_the_effective_configuration() {
        let home = tempfile::tempdir().expect("home");
        let _home_guard = crate::history::redirect_test_orca_home(home.path());
        std::fs::write(
            home.path().join("config.toml"),
            "[rewind]\nenabled = false\nobserve_untracked = false\nmax_checkpoints = 7\n",
        )
        .expect("config file");
        let workspace = tempfile::tempdir().expect("workspace");
        let settings = RewindSettings::load(workspace.path());
        assert!(!settings.enabled);
        assert!(!settings.observe_untracked);
        assert_eq!(settings.max_checkpoints, 7);
        assert!(!is_rewind_enabled(workspace.path()));
    }

    #[test]
    fn retention_eviction_garbage_collects_only_evicted_blobs() {
        let home = tempfile::tempdir().expect("home");
        let _home_guard = crate::history::redirect_test_orca_home(home.path());
        let cwd = tempfile::tempdir().expect("workspace");
        let mut writer = SessionWriter::start(
            cwd.path(),
            "mock",
            Some("mock-model".to_string()),
            "rewind retention gc",
        )
        .expect("writer");
        let session_id = writer.session_id().expect("session id").to_string();
        let transcript = writer.path().to_path_buf();
        let store = RewindStore::open(
            &home
                .path()
                .join("rewind")
                .join("sessions")
                .join(&session_id),
        )
        .expect("store");
        let root = cwd
            .path()
            .canonicalize()
            .expect("canonical workspace")
            .to_string_lossy()
            .into_owned();

        let mut blobs = Vec::new();
        for ordinal in 0..3_u32 {
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            let message = Message::user(format!("prompt {ordinal}"));
            writer.append_message(&message).expect("user message");
            let message_id = writer.last_committed_message_id().expect("committed id");
            let blob = store
                .put_bytes(format!("before {ordinal}\n").as_bytes(), None)
                .expect("pre-image blob");
            let mut files = BTreeMap::new();
            files.insert(
                format!("file{ordinal}.txt"),
                SnapshotFileEntry {
                    pre: Some(BlobRef {
                        sha256: blob.sha256.clone(),
                        size: blob.size,
                        mode: blob.mode,
                        encoding: "raw".to_string(),
                    }),
                    post_sha256: None,
                },
            );
            writer
                .append_file_snapshot(FileSnapshotRecord {
                    schema_version: REWIND_SCHEMA_VERSION,
                    snapshot_id: format!("snap_{ordinal}"),
                    session_id: session_id.clone(),
                    at_message_id: message_id,
                    turn_id: None,
                    ordinal,
                    root: root.clone(),
                    workspace_fingerprint: None,
                    complete: true,
                    start_digest: None,
                    end_digest: None,
                    files,
                    change_stats: None,
                    untracked: Vec::new(),
                    limitations: Vec::new(),
                    recorded_at: Utc::now(),
                })
                .expect("snapshot record");
            blobs.push(blob);
        }

        enforce_snapshot_retention_with_limit(&mut writer, &session_id, 2)
            .expect("retention watermark and GC");

        let retention = crate::thread_store::read_snapshot_retention(&transcript)
            .expect("read retention")
            .expect("watermark");
        assert_eq!(retention.evicted_through_ordinal, 0);
        assert!(
            store.read_blob(&blobs[0]).is_err(),
            "the evicted snapshot's pre-image is collected"
        );
        assert_eq!(
            store.read_blob(&blobs[1]).expect("retained blob"),
            b"before 1\n"
        );
        assert_eq!(
            store.read_blob(&blobs[2]).expect("retained blob"),
            b"before 2\n"
        );
    }

    #[test]
    fn restoring_files_into_a_different_workspace_is_refused() {
        let home = tempfile::tempdir().expect("home");
        let _home_guard = crate::history::redirect_test_orca_home(home.path());
        let session_cwd = tempfile::tempdir().expect("session workspace");
        let other_cwd = tempfile::tempdir().expect("other workspace");
        let mut writer = SessionWriter::start(
            session_cwd.path(),
            "mock",
            Some("mock-model".to_string()),
            "rewind cwd drift",
        )
        .expect("writer");
        writer.enter_turn(orca_core::thread_identity::TurnId::new());
        writer
            .append_message(&Message::user("edit a file".to_string()))
            .expect("user message");
        let message_id = writer
            .last_committed_message_id()
            .expect("committed message id");
        let snapshot_root = other_cwd
            .path()
            .canonicalize()
            .expect("canonical other workspace")
            .to_string_lossy()
            .into_owned();
        writer
            .append_file_snapshot(FileSnapshotRecord {
                schema_version: REWIND_SCHEMA_VERSION,
                snapshot_id: "snap_drift".to_string(),
                session_id: writer.session_id().expect("session id").to_string(),
                at_message_id: message_id.clone(),
                turn_id: None,
                ordinal: 0,
                root: snapshot_root,
                workspace_fingerprint: None,
                complete: true,
                start_digest: None,
                end_digest: None,
                files: BTreeMap::new(),
                change_stats: None,
                untracked: Vec::new(),
                limitations: Vec::new(),
                recorded_at: Utc::now(),
            })
            .expect("snapshot record");
        let session_id = writer.session_id().expect("session id").to_string();
        drop(writer);
        let transcript = crate::history::load_session(&session_id).expect("transcript");
        let mut config = RunConfig::default();
        config.cwd = Some(other_cwd.path().to_path_buf());
        config.history_mode = HistoryMode::Resume(session_id.clone());
        let mut session =
            InteractiveSession::new_with_preloaded(&config, "rewind cwd drift", Some(transcript))
                .expect("resumed session");

        let error = apply_rewind(&mut session, &config, &message_id, RewindMode::Code, true)
            .expect_err("a moved workspace must not receive the restore");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("does not match the checkpoint workspace"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn prompt_previews_are_first_line_and_bounded() {
        assert_eq!(prompt_preview("hello\nworld"), "hello");
        let long = "x".repeat(120);
        let preview = prompt_preview(&long);
        assert_eq!(preview.chars().count(), PROMPT_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }
}
