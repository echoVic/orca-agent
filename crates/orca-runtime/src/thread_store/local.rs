use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{ActivePermissionProfile, AdditionalWorkingDirectory};
use orca_core::tool_types::truncate_output;
use orca_platform::process::ProcessJob;

use super::LiveThread;
#[cfg(not(test))]
#[cfg(not(test))]
use super::ORCA_HOME_ENV;
use super::pagination::{page_thread_items, page_thread_turns, page_vec};
use super::projection::{
    conversation_records_to_thread_items, conversation_records_to_thread_turns,
    is_surface_coordinator_record, normalized_stored_messages, stored_message_to_thread_json,
};
use super::session_index::{self, SessionSummaryPage};
use super::types::{
    SessionMeta, SessionRecord, SessionSummary, SessionTranscript, SortDirection,
    StoredConversationRecord, StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchHit,
    StoredThreadSearchPage, StoredThreadSummary, StoredThreadSummaryPage, StoredThreadTurnPage,
    ThreadListFilters, ThreadMetadataPatch, ThreadRelationFilter, ThreadSortKey, ThreadStore,
    TurnItemsView,
};
use super::writer::{
    acquire_file_lock, conversation_record_from_semantic_event, open_regular_history_file,
    read_history_lines, read_records, read_session_meta, read_transcript, read_transcript_until,
    rewrite_records_unlocked, write_durable_record,
};

#[derive(Clone, Debug, Default)]
pub struct JsonlThreadStore;

pub type SessionStore = JsonlThreadStore;

impl JsonlThreadStore {
    pub(crate) fn append_surface_commit_prepared(
        &self,
        path: &Path,
        commit_id: String,
        event_count: u32,
        batch_digest: Vec<u8>,
        cursor_before: u64,
        cursor_after: u64,
        durable_revision: u64,
        batch: crate::runtime_surface::StoredSurfaceCommitBatchV1,
    ) -> io::Result<()> {
        write_durable_record(
            path,
            &SessionRecord::SurfaceCommitPrepared {
                commit_id,
                event_count,
                batch_digest,
                cursor_before,
                cursor_after,
                durable_revision,
                batch: Some(batch),
            },
        )
    }

    pub(crate) fn append_surface_commit_committed(
        &self,
        path: &Path,
        commit_id: String,
        event_count: u32,
        batch_digest: Vec<u8>,
        cursor_after: u64,
        durable_revision: u64,
    ) -> io::Result<()> {
        write_durable_record(
            path,
            &SessionRecord::SurfaceCommitCommitted {
                commit_id,
                event_count,
                batch_digest,
                cursor_after,
                durable_revision,
            },
        )
    }

    pub(crate) fn append_surface_owner_epoch(
        &self,
        path: &Path,
        owner_epoch: u64,
    ) -> io::Result<()> {
        write_durable_record(path, &SessionRecord::SurfaceOwnerEpoch { owner_epoch })
    }

    pub(crate) fn append_surface_finalize_intent(
        &self,
        path: &Path,
        finalize_intent_id: crate::runtime_surface::SurfaceFinalizeIntentId,
        expected_settlements: Vec<crate::runtime_surface::SurfaceSettlementId>,
    ) -> io::Result<()> {
        write_durable_record(
            path,
            &SessionRecord::SurfaceFinalizeIntent {
                finalize_intent_id,
                expected_settlements,
            },
        )
    }

    pub(crate) fn append_surface_settlement(
        &self,
        path: &Path,
        settlement_id: String,
        receipt_digest: Vec<u8>,
    ) -> io::Result<()> {
        write_durable_record(
            path,
            &SessionRecord::SurfaceSettlement {
                settlement_id,
                receipt_digest,
            },
        )
    }

    pub(crate) fn append_surface_shutdown_barrier(
        &self,
        path: &Path,
        barrier_id: String,
        plan_digest: Vec<u8>,
        record: crate::runtime_surface::StoredShutdownBarrierRecordV1,
    ) -> io::Result<()> {
        write_durable_record(
            path,
            &SessionRecord::SurfaceShutdownBarrier {
                barrier_id,
                plan_digest,
                record,
            },
        )
    }

    pub(crate) fn probe_surface_control_record(
        &self,
        path: &Path,
        requested_type: &str,
        requested_id: &str,
    ) -> io::Result<
        Option<(
            Vec<u8>,
            crate::runtime_surface::StoredShutdownBarrierRecordV1,
        )>,
    > {
        if !path.exists() {
            return Ok(None);
        }
        let mut found = None;
        for record in read_records(path)? {
            match record {
                SessionRecord::SurfaceShutdownBarrier {
                    barrier_id,
                    plan_digest,
                    record,
                } if requested_type == "shutdown" && barrier_id == requested_id => {
                    found = Some((plan_digest, record));
                }
                _ => {}
            }
        }
        Ok(found)
    }

    pub(crate) fn probe_surface_finalize_intent(
        &self,
        path: &Path,
        requested_id: &crate::runtime_surface::SurfaceFinalizeIntentId,
    ) -> io::Result<Option<Vec<crate::runtime_surface::SurfaceSettlementId>>> {
        if !path.exists() {
            return Ok(None);
        }
        let mut found = None;
        for record in read_records(path)? {
            if let SessionRecord::SurfaceFinalizeIntent {
                finalize_intent_id,
                expected_settlements,
            } = record
                && &finalize_intent_id == requested_id
            {
                found = Some(expected_settlements);
            }
        }
        Ok(found)
    }

    pub(crate) fn probe_surface_commit(
        &self,
        path: &Path,
        requested_commit_id: &str,
    ) -> io::Result<Option<(bool, u32, Vec<u8>, u64, u64, u64)>> {
        if !path.exists() {
            return Ok(None);
        }
        let mut found = None;
        for record in read_records(path)? {
            match record {
                SessionRecord::SurfaceCommitPrepared {
                    commit_id,
                    event_count,
                    batch_digest,
                    cursor_before,
                    cursor_after,
                    durable_revision,
                    batch: _,
                } if commit_id == requested_commit_id => {
                    found = Some((
                        false,
                        event_count,
                        batch_digest,
                        cursor_before,
                        cursor_after,
                        durable_revision,
                    ));
                }
                SessionRecord::SurfaceCommitCommitted {
                    commit_id,
                    event_count,
                    batch_digest,
                    cursor_after,
                    durable_revision,
                } if commit_id == requested_commit_id => {
                    found = Some((
                        true,
                        event_count,
                        batch_digest,
                        0,
                        cursor_after,
                        durable_revision,
                    ));
                }
                _ => {}
            }
        }
        Ok(found)
    }

    pub(crate) fn load_surface_commit_batches(
        &self,
        path: &Path,
    ) -> io::Result<Vec<(bool, crate::runtime_surface::StoredSurfaceCommitBatchV1)>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut batches = Vec::<(
            String,
            bool,
            crate::runtime_surface::StoredSurfaceCommitBatchV1,
        )>::new();
        for record in read_records(path)? {
            match record {
                SessionRecord::SurfaceCommitPrepared {
                    commit_id,
                    batch: Some(batch),
                    ..
                } => {
                    if let Some(existing) = batches
                        .iter_mut()
                        .find(|(existing_id, _, _)| existing_id == &commit_id)
                    {
                        existing.2 = batch;
                    } else {
                        batches.push((commit_id, false, batch));
                    }
                }
                SessionRecord::SurfaceCommitPrepared { batch: None, .. } => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "surface commit is missing its durable batch payload",
                    ));
                }
                SessionRecord::SurfaceCommitCommitted { commit_id, .. } => {
                    let Some(existing) = batches
                        .iter_mut()
                        .find(|(existing_id, _, _)| existing_id == &commit_id)
                    else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "committed surface record has no prepared batch",
                        ));
                    };
                    existing.1 = true;
                }
                _ => {}
            }
        }
        Ok(batches
            .into_iter()
            .map(|(_, committed, batch)| (committed, batch))
            .collect())
    }

    pub fn list_sessions(&self, limit: usize) -> io::Result<Vec<SessionSummary>> {
        list_sessions(limit)
    }

    pub fn list_sessions_with_archived(
        &self,
        limit: usize,
        include_archived: bool,
    ) -> io::Result<Vec<SessionSummary>> {
        list_sessions_with_archived(limit, include_archived)
    }

    pub fn load_session(&self, selector: &str) -> io::Result<SessionTranscript> {
        load_session(selector)
    }

    pub fn load_session_until(
        &self,
        selector: &str,
        boundary_message_id: &str,
    ) -> io::Result<SessionTranscript> {
        load_session_until(selector, boundary_message_id)
    }

    pub fn search_sessions(
        &self,
        query: &str,
        include_archived: bool,
    ) -> io::Result<Vec<SearchHit>> {
        search_sessions(query, include_archived)
    }

    pub fn delete_session(&self, selector: &str) -> io::Result<PathBuf> {
        delete_session(selector)
    }

    pub fn archive_session(&self, selector: &str) -> io::Result<PathBuf> {
        archive_session(selector)
    }

    pub fn rename_session(&self, selector: &str, title: &str) -> io::Result<PathBuf> {
        rename_session(selector, title)
    }

    pub fn compress_session(&self, selector: &str) -> io::Result<PathBuf> {
        compress_session(selector)
    }
}

pub fn delete_session(selector: &str) -> io::Result<PathBuf> {
    let path = if is_latest_selector(selector) {
        list_sessions_with_archived(1, true)?
            .into_iter()
            .next()
            .map(|session| session.path)
    } else {
        find_session_path(selector, true)?
    }
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{selector}'"),
        )
    })?;
    fs::remove_file(&path)?;
    #[cfg(not(test))]
    let _ = session_index::remove_path(&path);
    Ok(path)
}

pub fn archive_session(selector: &str) -> io::Result<PathBuf> {
    let path = if is_latest_selector(selector) {
        list_sessions(1)?
            .into_iter()
            .next()
            .map(|session| session.path)
    } else {
        find_session_path(selector, false)?
    }
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no active session matches '{selector}'"),
        )
    })?;
    let relative = path.strip_prefix(sessions_dir()).unwrap_or(&path);
    let archived_path = archive_dir().join(relative);
    if let Some(parent) = archived_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&path, &archived_path)?;
    #[cfg(not(test))]
    let _ = session_index::move_path(&path, &archived_path, true);
    Ok(archived_path)
}

pub fn rename_session(selector: &str, title: &str) -> io::Result<PathBuf> {
    let path = resolve_session_path(selector, true)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{selector}'"),
        )
    })?;
    let meta = read_session_meta(&path)?;
    JsonlThreadStore::new().update_thread_metadata(
        &meta.session_id,
        ThreadMetadataPatch {
            title: Some(title.to_string()),
            ..ThreadMetadataPatch::default()
        },
    )?;
    Ok(path)
}

pub fn compress_session(selector: &str) -> io::Result<PathBuf> {
    let path = resolve_session_path(selector, true)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{selector}'"),
        )
    })?;
    if path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        return Ok(path);
    }
    let compressed_path = path.with_extension("jsonl.zst");
    let _lock = acquire_file_lock(&path)?;
    let result = (|| {
        let input = open_regular_history_file(&path)?;
        let output = File::create(&compressed_path)?;
        if let Err(error) = zstd::stream::copy_encode(input, output, 3) {
            let _ = fs::remove_file(&compressed_path);
            return Err(io::Error::other(error));
        }
        fs::remove_file(&path)?;
        #[cfg(not(test))]
        {
            let archived = compressed_path.starts_with(archive_dir());
            let _ = session_index::move_path(&path, &compressed_path, archived);
        }
        Ok(compressed_path)
    })();
    result
}

pub(crate) fn load_thread_records(
    thread_id: &str,
) -> io::Result<(SessionMeta, Vec<StoredConversationRecord>)> {
    let path = find_session_path(thread_id, true)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{thread_id}'"),
        )
    })?;
    let records = read_records(&path)?;
    let mut meta = None;
    let mut conversation_records = Vec::new();
    for record in records {
        if is_surface_coordinator_record(&record) {
            continue;
        }
        match record {
            SessionRecord::Meta(record_meta) => meta = Some(record_meta),
            SessionRecord::Message {
                id,
                turn_id,
                message,
            } => conversation_records.push(StoredConversationRecord {
                item_id: id,
                turn_id,
                message,
                completed_model_items: None,
            }),
            SessionRecord::SemanticEvent { event } => {
                if let Some(record) = conversation_record_from_semantic_event(&event)? {
                    conversation_records.push(record);
                }
            }
            _ => {}
        }
    }
    let meta = meta.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session '{thread_id}' is missing metadata"),
        )
    })?;
    Ok((meta, conversation_records))
}

pub fn list_sessions(limit: usize) -> io::Result<Vec<SessionSummary>> {
    list_sessions_with_archived(limit, false)
}

pub fn list_session_page(
    offset: usize,
    limit: usize,
    include_archived: bool,
    search_term: Option<&str>,
) -> io::Result<SessionSummaryPage> {
    session_index::list_page(offset, limit, include_archived, search_term)
}

pub fn list_sessions_with_archived(
    limit: usize,
    include_archived: bool,
) -> io::Result<Vec<SessionSummary>> {
    if limit == usize::MAX {
        session_index::ensure_backfill_complete()?;
    }
    Ok(list_session_page(0, limit, include_archived, None)?.sessions)
}

pub fn load_session(selector: &str) -> io::Result<SessionTranscript> {
    let path = if is_latest_selector(selector) {
        list_sessions(1)?
            .into_iter()
            .next()
            .map(|s| s.path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no saved sessions"))?
    } else {
        find_session_path(selector, true)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no saved session matches '{selector}'"),
            )
        })?
    };

    read_transcript(&path)
}

/// Like [`load_session`], but restores only the message log up to the
/// persisted conversation item id `boundary_message_id` (inclusive).
pub fn load_session_until(
    selector: &str,
    boundary_message_id: &str,
) -> io::Result<SessionTranscript> {
    let path = if is_latest_selector(selector) {
        list_sessions(1)?
            .into_iter()
            .next()
            .map(|s| s.path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no saved sessions"))?
    } else {
        find_session_path(selector, true)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no saved session matches '{selector}'"),
            )
        })?
    };

    read_transcript_until(&path, boundary_message_id)
}

pub(crate) fn summarize_session_with_archive_flag(
    path: &Path,
    archived: bool,
) -> io::Result<SessionSummary> {
    let meta = read_session_meta(path)?;
    let updated_at = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or(meta.created_at);

    Ok(SessionSummary {
        session_id: meta.session_id,
        title: meta.title,
        cwd: meta.cwd,
        provider: meta.provider,
        model: meta.model,
        created_at: meta.created_at,
        updated_at,
        path: path.to_path_buf(),
        archived,
        parent_id: meta.parent_id,
        forked: meta.forked,
        approval_mode: meta.approval_mode,
        active_permission_profile: meta.active_permission_profile,
        permission_rule_count: meta.permission_rules.rules.len(),
        runtime_workspace_roots: meta.runtime_workspace_roots,
        additional_working_directories: meta.additional_working_directories,
        network_domain_permissions: meta.network_domain_permissions,
    })
}

pub fn search_sessions(query: &str, include_archived: bool) -> io::Result<Vec<SearchHit>> {
    let mut hits = Vec::new();
    let used_ripgrep = search_roots_with_ripgrep(query, include_archived, &mut hits)?;
    let mut seen: HashSet<(PathBuf, usize)> = hits
        .iter()
        .map(|hit| (hit.path.clone(), hit.line_number))
        .collect();

    if !used_ripgrep {
        search_root_in_process(&sessions_dir(), false, query, &mut hits, &mut seen)?;
    } else {
        search_compressed_root(&sessions_dir(), false, query, &mut hits, &mut seen)?;
    }
    if include_archived {
        if !used_ripgrep {
            search_root_in_process(&archive_dir(), true, query, &mut hits, &mut seen)?;
        } else {
            search_compressed_root(&archive_dir(), true, query, &mut hits, &mut seen)?;
        }
    }
    Ok(hits)
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub archived: bool,
    pub path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

const THREAD_SEARCH_TIMEOUT: Duration = Duration::from_secs(120);
const THREAD_SEARCH_MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const THREAD_SEARCH_MAX_SNIPPET_BYTES: usize = 8 * 1024;
const THREAD_SEARCH_MAX_PROCESS_HITS: usize = 4_096;

struct RipgrepSearchMatch {
    path: PathBuf,
    archived: bool,
    line_number: usize,
    line: String,
}

struct RipgrepSearchCollector {
    archive_root: PathBuf,
    matches: Vec<RipgrepSearchMatch>,
}

impl RipgrepSearchCollector {
    fn new() -> Self {
        Self {
            archive_root: archive_dir(),
            matches: Vec::new(),
        }
    }

    fn push(&mut self, line: orca_tools::process::BoundedLine<'_>) {
        if line.omitted_bytes > 0 || self.matches.len() >= THREAD_SEARCH_MAX_PROCESS_HITS {
            return;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line.bytes) else {
            return;
        };
        if value["type"].as_str() != Some("match") {
            return;
        }
        let Some(path_text) = value["data"]["path"]["text"].as_str() else {
            return;
        };
        let Some(line_number) = value["data"]["line_number"].as_u64() else {
            return;
        };
        let Some(line_text) = value["data"]["lines"]["text"].as_str() else {
            return;
        };
        let path = PathBuf::from(path_text);
        let (line, _) = truncate_output(
            line_text.trim_end_matches('\n').to_string(),
            THREAD_SEARCH_MAX_SNIPPET_BYTES,
        );
        self.matches.push(RipgrepSearchMatch {
            archived: path.starts_with(&self.archive_root),
            path,
            line_number: line_number as usize,
            line,
        });
    }
}

fn search_roots_with_ripgrep(
    query: &str,
    include_archived: bool,
    hits: &mut Vec<SearchHit>,
) -> io::Result<bool> {
    let mut roots = Vec::new();
    if sessions_dir().exists() {
        roots.push(sessions_dir());
    }
    if include_archived && archive_dir().exists() {
        roots.push(archive_dir());
    }
    if roots.is_empty() {
        return Ok(true);
    }

    let mut command = Command::new("rg");
    command
        .arg("--json")
        .arg("--fixed-strings")
        .arg("--glob")
        .arg("*.jsonl")
        .arg(query)
        .args(&roots)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    orca_tools::process::prepare_non_interactive_command(&mut command);
    let (child, process_job) = match ProcessJob::spawn(&mut command) {
        Ok(spawned) => spawned,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let output = orca_tools::process::wait_for_child_stdout_lines_with_timeout(
        child,
        process_job,
        THREAD_SEARCH_TIMEOUT,
        THREAD_SEARCH_MAX_JSON_LINE_BYTES,
        RipgrepSearchCollector::new(),
        |collector, line| {
            collector.push(line);
            Ok(())
        },
    )?;

    if output.timed_out {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "thread search timed out after {}s",
                THREAD_SEARCH_TIMEOUT.as_secs()
            ),
        ));
    }

    if !output.status.success() && output.status.code() != Some(1) {
        return Ok(false);
    }

    for matched in output.value.matches {
        push_search_hit(
            &matched.path,
            matched.archived,
            matched.line_number,
            matched.line,
            hits,
        );
    }

    Ok(true)
}

fn search_root_in_process(
    root: &Path,
    archived: bool,
    query: &str,
    hits: &mut Vec<SearchHit>,
    seen: &mut HashSet<(PathBuf, usize)>,
) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        if let Ok(lines) = read_history_lines(path) {
            push_matching_lines(path, archived, query, &lines, hits, seen);
        }
    })
}

fn search_compressed_root(
    root: &Path,
    archived: bool,
    query: &str,
    hits: &mut Vec<SearchHit>,
    seen: &mut HashSet<(PathBuf, usize)>,
) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        if path.extension().and_then(|ext| ext.to_str()) != Some("zst") {
            return;
        }
        if let Ok(lines) = read_history_lines(path) {
            push_matching_lines(path, archived, query, &lines, hits, seen);
        }
    })
}

fn push_matching_lines(
    path: &Path,
    archived: bool,
    query: &str,
    lines: &[String],
    hits: &mut Vec<SearchHit>,
    seen: &mut HashSet<(PathBuf, usize)>,
) {
    for (index, line) in lines.iter().enumerate() {
        let line_number = index + 1;
        if line.contains(query) && seen.insert((path.to_path_buf(), line_number)) {
            push_search_hit(path, archived, line_number, line.clone(), hits);
        }
    }
}

fn push_search_hit(
    path: &Path,
    archived: bool,
    line_number: usize,
    line: String,
    hits: &mut Vec<SearchHit>,
) {
    if let Ok(meta) = read_session_meta(path) {
        hits.push(SearchHit {
            session_id: meta.session_id,
            title: meta.title,
            archived,
            path: path.to_path_buf(),
            line_number,
            line,
        });
    }
}

pub(crate) fn find_session_path(
    selector: &str,
    include_archived: bool,
) -> io::Result<Option<PathBuf>> {
    #[cfg(not(test))]
    if let Ok(Some(path)) = session_index::find_path(selector, include_archived)
        && path.exists()
    {
        return Ok(Some(path));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    collect_matching_paths(&sessions_dir(), selector, &mut candidates)?;
    if include_archived {
        collect_matching_paths(&archive_dir(), selector, &mut candidates)?;
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    candidates.sort_by(|a, b| b.cmp(a));
    Ok(Some(candidates.into_iter().next().unwrap()))
}

pub(crate) fn resolve_session_path(
    selector: &str,
    include_archived: bool,
) -> io::Result<Option<PathBuf>> {
    if is_latest_selector(selector) {
        return Ok(list_sessions_with_archived(1, include_archived)?
            .into_iter()
            .next()
            .map(|session| session.path));
    }
    find_session_path(selector, include_archived)
}

fn collect_matching_paths(
    root: &Path,
    selector: &str,
    candidates: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if is_latest_selector(selector) {
        return Ok(());
    }
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        if file_name.contains(selector) {
            candidates.push(path.to_path_buf());
        }
    })
}

pub(crate) fn is_latest_selector(selector: &str) -> bool {
    matches!(selector, "latest" | "last")
}

pub(crate) fn collect_session_files(dir: &Path, on_file: &mut dyn FnMut(&Path)) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_session_files(&path, on_file)?;
        } else if file_type.is_file() && is_history_file(&path) {
            on_file(&path);
        }
    }
    Ok(())
}

pub(crate) fn is_regular_history_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
        && is_history_file(path)
}

fn is_history_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")
}

pub(crate) fn sessions_dir() -> PathBuf {
    orca_home().join("sessions")
}

pub(crate) fn archive_dir() -> PathBuf {
    orca_home().join("archive")
}

pub(crate) fn orca_home() -> PathBuf {
    // Under test, resolve through the thread-local per-test home override
    // (host threads, then the calling test thread), falling back to the
    // process-wide ORCA_HOME variable; the environment is never redirected.
    #[cfg(test)]
    let test_home = crate::history::read_test_orca_home();
    #[cfg(not(test))]
    let test_home = std::env::var_os(ORCA_HOME_ENV);
    test_home
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .unwrap_or_else(|| std::env::temp_dir().join("orca"))
}

impl ThreadStore for JsonlThreadStore {
    fn create_live_thread(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<LiveThread> {
        let meta = self.create_meta(cwd, provider, model, prompt);
        let thread_id = meta.session_id.clone();
        let writer = self.start_writer_from_meta(meta)?;
        Ok(LiveThread { thread_id, writer })
    }

    fn create_live_thread_with_permissions(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_mode: ApprovalMode,
        permission_rules: PermissionRules,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
    ) -> io::Result<LiveThread> {
        let meta = self.create_meta_with_permissions(
            cwd,
            provider,
            model,
            prompt,
            active_permission_profile,
            approval_mode,
            permission_rules,
            additional_working_directories,
        );
        let thread_id = meta.session_id.clone();
        let writer = self.start_writer_from_meta(meta)?;
        Ok(LiveThread { thread_id, writer })
    }

    fn update_thread_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<SessionSummary> {
        let path = find_session_path(thread_id, true)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no saved session matches '{thread_id}'"),
            )
        })?;
        let _lock = acquire_file_lock(&path)?;
        let mut records = read_records(&path)?;
        let mut patched = false;
        for record in &mut records {
            if let SessionRecord::Meta(meta) = record {
                if let Some(title) = patch.title {
                    meta.title = title;
                    patched = true;
                }
                if let Some(approval_mode) = patch.approval_mode {
                    meta.approval_mode = Some(approval_mode);
                    patched = true;
                }
                if let Some(active_permission_profile) = patch.active_permission_profile {
                    meta.active_permission_profile = Some(active_permission_profile);
                    patched = true;
                }
                if let Some(runtime_workspace_roots) = patch.runtime_workspace_roots {
                    meta.runtime_workspace_roots = runtime_workspace_roots;
                    patched = true;
                }
                if let Some(permission_rules) = patch.permission_rules {
                    meta.permission_rules = permission_rules;
                    patched = true;
                }
                if let Some(additional_working_directories) = patch.additional_working_directories {
                    meta.additional_working_directories = additional_working_directories;
                    patched = true;
                }
                if let Some(metadata_writable_directories) = patch.metadata_writable_directories {
                    meta.metadata_writable_directories = metadata_writable_directories;
                    patched = true;
                }
                if let Some(network_domain_permissions) = patch.network_domain_permissions {
                    meta.network_domain_permissions = network_domain_permissions;
                    patched = true;
                }
                if let Some(unsandboxed_shell) = patch.unsandboxed_shell {
                    meta.unsandboxed_shell = unsandboxed_shell;
                    patched = true;
                }
                break;
            }
        }
        if !patched {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread metadata patch did not include any supported fields",
            ));
        }
        rewrite_records_unlocked(&path, &records)?;
        let summary = summarize_session_with_archive_flag(&path, path.starts_with(archive_dir()))?;
        #[cfg(not(test))]
        let _ = session_index::upsert_summary(&summary);
        Ok(summary)
    }

    fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection> {
        let (meta, conversation_records) = load_thread_records(thread_id)?;
        let stored_messages = normalized_stored_messages(&conversation_records);
        let projected_messages = if include_messages {
            stored_messages
                .iter()
                .map(stored_message_to_thread_json)
                .collect()
        } else {
            Vec::new()
        };
        let turns = if include_turns {
            conversation_records_to_thread_turns(
                &meta.session_id,
                &conversation_records,
                usize::MAX,
                TurnItemsView::Full,
            )?
        } else {
            Vec::new()
        };
        Ok(StoredThreadProjection {
            thread_id: meta.session_id,
            title: meta.title,
            cwd: meta.cwd,
            runtime_workspace_roots: meta.runtime_workspace_roots,
            active_permission_profile: meta.active_permission_profile,
            additional_working_directories: meta.additional_working_directories,
            metadata_writable_directories: meta.metadata_writable_directories,
            network_domain_permissions: meta.network_domain_permissions,
            unsandboxed_shell: meta.unsandboxed_shell,
            message_count: stored_messages.len(),
            messages: projected_messages,
            turns,
        })
    }

    fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage> {
        let mut summaries = self
            .list_sessions_with_archived(usize::MAX, filters.archived)?
            .into_iter()
            .map(StoredThreadSummary::from)
            .collect::<Vec<_>>();
        let all_summaries = summaries.clone();
        summaries
            .retain(|summary| thread_summary_matches_filters(summary, &filters, &all_summaries));
        if let Some(search_term) = search_term.filter(|term| !term.is_empty()) {
            summaries.retain(|summary| thread_summary_matches(summary, search_term));
        }
        sort_thread_summaries(&mut summaries, sort_key);
        if sort_direction == SortDirection::Asc {
            summaries.reverse();
        }
        let (data, next_cursor, backwards_cursor) = page_vec(summaries, cursor, limit);
        Ok(StoredThreadSummaryPage {
            data,
            next_cursor,
            backwards_cursor,
        })
    }

    fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage> {
        let mut hits = self
            .search_sessions(query, include_archived)?
            .into_iter()
            .map(|hit| {
                let archived = hit.archived;
                let snippet = hit.line.clone();
                summarize_session_with_archive_flag(&hit.path, archived).map(|summary| {
                    StoredThreadSearchHit {
                        thread: StoredThreadSummary::from(summary),
                        snippet,
                    }
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        sort_thread_search_hits(&mut hits, sort_key);
        if sort_direction == SortDirection::Asc {
            hits.reverse();
        }
        let (data, next_cursor, backwards_cursor) = page_vec(hits, cursor, limit);
        Ok(StoredThreadSearchPage {
            data,
            next_cursor,
            backwards_cursor,
        })
    }

    fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage> {
        let (meta, records) = load_thread_records(thread_id)?;
        Ok(page_thread_turns(
            conversation_records_to_thread_turns(
                &meta.session_id,
                &records,
                usize::MAX,
                items_view,
            )?,
            cursor,
            limit,
            sort_direction,
        ))
    }

    fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage> {
        let (meta, records) = load_thread_records(thread_id)?;
        Ok(page_thread_items(
            conversation_records_to_thread_items(&meta.session_id, &records, turn_id, usize::MAX)?,
            cursor,
            limit,
            sort_direction,
        ))
    }
}

pub(crate) fn thread_summary_matches(summary: &StoredThreadSummary, search_term: &str) -> bool {
    summary.title.contains(search_term)
        || summary.cwd.contains(search_term)
        || summary.provider.contains(search_term)
        || summary
            .model
            .as_deref()
            .is_some_and(|model| model.contains(search_term))
}

pub(crate) fn thread_summary_matches_filters(
    summary: &StoredThreadSummary,
    filters: &ThreadListFilters,
    all_summaries: &[StoredThreadSummary],
) -> bool {
    if summary.archived != filters.archived {
        return false;
    }
    if let Some(model_providers) = &filters.model_providers {
        if !model_providers.is_empty()
            && !model_providers
                .iter()
                .any(|provider| provider == &summary.provider)
        {
            return false;
        }
    }
    if let Some(model_names) = &filters.model_names {
        if !model_names.is_empty()
            && !summary
                .model
                .as_ref()
                .is_some_and(|model| model_names.iter().any(|expected| expected == model))
        {
            return false;
        }
    }
    if !filters.cwd_filters.is_empty() && !filters.cwd_filters.iter().any(|cwd| cwd == &summary.cwd)
    {
        return false;
    }
    match &filters.relation {
        Some(ThreadRelationFilter::DirectChildrenOf(parent_id)) => {
            summary.parent_id.as_deref() == Some(parent_id.as_str())
        }
        Some(ThreadRelationFilter::DescendantsOf(ancestor_id)) => {
            thread_descends_from(summary, ancestor_id, all_summaries)
        }
        None => true,
    }
}

fn thread_descends_from(
    summary: &StoredThreadSummary,
    ancestor_id: &str,
    all_summaries: &[StoredThreadSummary],
) -> bool {
    let mut next_parent = summary.parent_id.as_deref();
    while let Some(parent_id) = next_parent {
        if parent_id == ancestor_id {
            return true;
        }
        next_parent = all_summaries
            .iter()
            .find(|candidate| candidate.thread_id == parent_id)
            .and_then(|candidate| candidate.parent_id.as_deref());
    }
    false
}

pub(crate) fn sort_thread_summaries(
    summaries: &mut [StoredThreadSummary],
    sort_key: ThreadSortKey,
) {
    summaries.sort_by(|a, b| match sort_key {
        ThreadSortKey::CreatedAt => b
            .created_at
            .cmp(&a.created_at)
            .then_with(|| b.updated_at.cmp(&a.updated_at)),
        ThreadSortKey::UpdatedAt | ThreadSortKey::RecencyAt => b
            .updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at)),
    });
}

pub(crate) fn sort_thread_search_hits(hits: &mut [StoredThreadSearchHit], sort_key: ThreadSortKey) {
    hits.sort_by(|a, b| match sort_key {
        ThreadSortKey::CreatedAt => b
            .thread
            .created_at
            .cmp(&a.thread.created_at)
            .then_with(|| b.thread.updated_at.cmp(&a.thread.updated_at)),
        ThreadSortKey::UpdatedAt | ThreadSortKey::RecencyAt => b
            .thread
            .updated_at
            .cmp(&a.thread.updated_at)
            .then_with(|| b.thread.created_at.cmp(&a.thread.created_at)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn session_discovery_ignores_non_regular_history_entries() {
        use std::ffi::CString;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let regular = root.path().join("regular.jsonl");
        fs::write(&regular, b"{}\n").unwrap();

        let fifo = root.path().join("blocked.jsonl");
        let fifo_path = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);

        let symlink_path = root.path().join("linked.jsonl");
        symlink(&regular, &symlink_path).unwrap();

        let mut discovered = Vec::new();
        collect_session_files(root.path(), &mut |path| {
            discovered.push(path.to_path_buf());
        })
        .unwrap();
        discovered.sort();

        assert_eq!(discovered, vec![regular]);
    }

    fn match_line(path: &Path, line_text: &str, line_number: usize) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": "match",
            "data": {
                "path": { "text": path.display().to_string() },
                "line_number": line_number,
                "lines": { "text": line_text },
            }
        }))
        .expect("match json")
    }

    #[test]
    fn ripgrep_search_collector_bounds_snippets_and_hit_count() {
        let path = sessions_dir().join("thread.jsonl");
        let mut collector = RipgrepSearchCollector::new();
        let large = match_line(&path, &format!("needle {}", "x".repeat(32 * 1024)), 1);
        collector.push(orca_tools::process::BoundedLine {
            bytes: &large,
            observed_bytes: large.len(),
            omitted_bytes: 0,
        });

        assert_eq!(collector.matches.len(), 1);
        assert!(collector.matches[0].line.len() <= THREAD_SEARCH_MAX_SNIPPET_BYTES);
        assert!(
            collector.matches[0]
                .line
                .contains("tool output micro-compacted")
        );

        let small = match_line(&path, "needle", 2);
        for _ in 0..THREAD_SEARCH_MAX_PROCESS_HITS {
            collector.push(orca_tools::process::BoundedLine {
                bytes: &small,
                observed_bytes: small.len(),
                omitted_bytes: 0,
            });
        }
        assert_eq!(collector.matches.len(), THREAD_SEARCH_MAX_PROCESS_HITS);
    }

    #[test]
    fn ripgrep_search_collector_rejects_truncated_json_frames() {
        let mut collector = RipgrepSearchCollector::new();
        collector.push(orca_tools::process::BoundedLine {
            bytes: br#"{"type":"match""#,
            observed_bytes: THREAD_SEARCH_MAX_JSON_LINE_BYTES + 1,
            omitted_bytes: 1,
        });

        assert!(collector.matches.is_empty());
    }
}
