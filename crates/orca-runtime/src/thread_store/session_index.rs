use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

use super::local::{
    collect_session_files, is_regular_history_file, orca_home, storage_identity_for_path,
    summarize_session_with_archive_flag,
};
use super::types::{SessionMeta, SessionSummary, StoredSessionHealth, StoredSessionHealthIssue};

const DATABASE_FILENAME: &str = "sessions-index.sqlite3";
const SCHEMA_VERSION: i64 = 4;
const RECENT_SEED_LIMIT: usize = 20;

static BACKFILLS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static RECENT_TOUCHES: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct SessionSummaryPage {
    pub sessions: Vec<SessionSummary>,
    pub next_offset: Option<usize>,
    pub backfill_complete: bool,
}

pub(crate) fn list_page(
    offset: usize,
    limit: usize,
    include_archived: bool,
    search_term: Option<&str>,
) -> io::Result<SessionSummaryPage> {
    let home = orca_home();
    list_page_at(&home, offset, limit, include_archived, search_term)
}

fn list_page_at(
    home: &Path,
    offset: usize,
    limit: usize,
    include_archived: bool,
    search_term: Option<&str>,
) -> io::Result<SessionSummaryPage> {
    let mut connection = open_index(&home)?;
    seed_recent_if_empty(&mut connection, home, include_archived)?;
    spawn_backfill_if_needed(home.to_path_buf());

    let page_size = limit.max(1);
    let sql = "SELECT summary_json, path, archived, updated_at_ms
         FROM sessions
         WHERE (?1 = 1 OR archived = 0)
           AND (?2 = '' OR instr(lower(title), lower(?2)) > 0)
         ORDER BY updated_at_ms DESC, created_at_ms DESC, session_id DESC
         LIMIT ?3 OFFSET ?4";
    let mut statement = connection.prepare(sql).map_err(io::Error::other)?;
    let rows = statement
        .query_map(
            params![
                include_archived,
                search_term.unwrap_or_default(),
                i64::try_from(page_size.saturating_add(1)).unwrap_or(i64::MAX),
                i64::try_from(offset).unwrap_or(i64::MAX)
            ],
            |row| {
                let summary_json: String = row.get(0)?;
                let path: String = row.get(1)?;
                let archived: bool = row.get(2)?;
                let updated_at_ms: i64 = row.get(3)?;
                Ok((summary_json, path, archived, updated_at_ms))
            },
        )
        .map_err(io::Error::other)?;

    let mut sessions = Vec::with_capacity(page_size.saturating_add(1).min(1024));
    let mut stale_paths = Vec::new();
    let mut repaired_summaries = Vec::new();
    for row in rows {
        let (summary_json, path, archived, updated_at_ms) = row.map_err(io::Error::other)?;
        let path = PathBuf::from(path);
        if !is_regular_history_file(&path) {
            // A deleted, replaced, or unsafe entry is no longer a catalog
            // record. Parse failures are different: those remain visible.
            stale_paths.push(path);
            continue;
        }
        let cached_summary = serde_json::from_str::<SessionSummary>(&summary_json).ok();
        let mut summary = match summarize_session_with_archive_flag(&path, archived) {
            Ok(summary) => summary,
            Err(_error) => {
                // Keep the old catalog row visible even if a transient read
                // fails. The SQLite cache is rebuildable, so malformed cache
                // JSON must not turn one row into a whole-page failure.
                let mut cached = cached_summary
                    .clone()
                    .unwrap_or_else(|| unreadable_summary(&path, archived, updated_at_ms));
                cached.health = StoredSessionHealth::Quarantined;
                cached.health_issue = Some(StoredSessionHealthIssue {
                    code: "scan_failed".to_string(),
                    line: None,
                    offset: None,
                });
                cached.path = path.clone();
                cached.archived = archived;
                cached
            }
        };
        // The source can change after index insertion. Re-scan before
        // returning the row and repair derived health in place.
        let needs_repair = cached_summary.as_ref().is_none_or(|cached| {
            cached.source_fingerprint != summary.source_fingerprint
                || cached.health != summary.health
                || cached.health_issue != summary.health_issue
                || cached.storage_identity != summary.storage_identity
        });
        if needs_repair {
            repaired_summaries.push(summary.clone());
        }
        summary.path = path;
        summary.archived = archived;
        if let Some(updated_at) = DateTime::<Utc>::from_timestamp_millis(updated_at_ms) {
            summary.updated_at = updated_at;
        }
        sessions.push(summary);
    }
    drop(statement);

    for path in stale_paths {
        let _ = remove_path_with_connection(&connection, &path);
    }
    for summary in repaired_summaries {
        let _ = upsert_summary_with_connection(&connection, &summary);
    }

    let has_more = sessions.len() > page_size;
    sessions.truncate(page_size);
    let backfill_complete = backfill_complete(&connection)?;
    let next_offset =
        (has_more || !backfill_complete).then(|| offset.saturating_add(sessions.len()));
    Ok(SessionSummaryPage {
        sessions,
        next_offset,
        backfill_complete,
    })
}

pub(crate) fn upsert_meta(path: &Path, meta: &SessionMeta, archived: bool) -> io::Result<()> {
    let Some((home, inferred_archived)) = managed_home(path) else {
        return Ok(());
    };
    let archived = inferred_archived || archived;
    // This write-side fast path still scans the just-written bytes. Health is
    // derived cache data, never an assumption based solely on the metadata.
    let summary = summarize_session_with_archive_flag(path, archived)
        .or_else(|_| summary_from_meta(path, meta.clone(), archived))?;
    let connection = open_index(&home)?;
    upsert_summary_with_connection(&connection, &summary)
}

pub(crate) fn upsert_summary(summary: &SessionSummary) -> io::Result<()> {
    let Some((home, _)) = managed_home(&summary.path) else {
        return Ok(());
    };
    let connection = open_index(&home)?;
    upsert_summary_with_connection(&connection, summary)
}

pub(crate) fn touch_path(path: &Path) -> io::Result<()> {
    let now = Instant::now();
    let touches = RECENT_TOUCHES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut recent = touches
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if recent
        .get(path)
        .is_some_and(|previous| now.duration_since(*previous) < Duration::from_secs(1))
    {
        return Ok(());
    }
    recent.insert(path.to_path_buf(), now);
    drop(recent);
    touch_path_force(path)
}

pub(crate) fn touch_path_force(path: &Path) -> io::Result<()> {
    let Some((home, _)) = managed_home(path) else {
        return Ok(());
    };
    let connection = open_index(&home)?;
    let updated_at_ms = modified_millis(path).unwrap_or_else(now_millis);
    connection
        .execute(
            "UPDATE sessions SET updated_at_ms = ?1 WHERE path = ?2",
            params![updated_at_ms, path.to_string_lossy()],
        )
        .map_err(io::Error::other)?;
    Ok(())
}

pub(crate) fn move_path(old_path: &Path, new_path: &Path, archived: bool) -> io::Result<()> {
    clear_recent_touch(old_path);
    let Some((home, inferred_archived)) = managed_home(new_path) else {
        return Ok(());
    };
    let connection = open_index(&home)?;
    connection
        .execute(
            "UPDATE sessions
             SET path = ?1, archived = ?2, updated_at_ms = ?3
             WHERE path = ?4",
            params![
                new_path.to_string_lossy(),
                inferred_archived || archived,
                modified_millis(new_path).unwrap_or_else(now_millis),
                old_path.to_string_lossy()
            ],
        )
        .map_err(io::Error::other)?;
    Ok(())
}

pub(crate) fn remove_path(path: &Path) -> io::Result<()> {
    clear_recent_touch(path);
    let Some((home, _)) = managed_home(path) else {
        return Ok(());
    };
    let connection = open_index(&home)?;
    remove_path_with_connection(&connection, path)
}

fn clear_recent_touch(path: &Path) {
    RECENT_TOUCHES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(path);
}

fn managed_home(path: &Path) -> Option<(PathBuf, bool)> {
    path.ancestors().find_map(|ancestor| {
        let name = ancestor.file_name()?.to_str()?;
        let archived = match name {
            "sessions" => false,
            "archive" => true,
            _ => return None,
        };
        Some((ancestor.parent()?.to_path_buf(), archived))
    })
}

pub(crate) fn find_path(session_id: &str, include_archived: bool) -> io::Result<Option<PathBuf>> {
    let connection = open_index(&orca_home())?;
    let path = connection
        .query_row(
            "SELECT path FROM sessions
             WHERE (session_id = ?1 OR storage_identity = ?1)
               AND (?2 = 1 OR archived = 0)
             ORDER BY updated_at_ms DESC, created_at_ms DESC, path DESC
             LIMIT 1",
            params![session_id, include_archived],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map(|path| path.map(PathBuf::from))
        .map_err(io::Error::other)?;
    let Some(path) = path else {
        return Ok(None);
    };
    if is_regular_history_file(&path) {
        return Ok(Some(path));
    }
    remove_path_with_connection(&connection, &path)?;
    Ok(None)
}

pub(crate) fn ensure_backfill_complete() -> io::Result<()> {
    let home = orca_home();
    ensure_backfill_complete_at(&home)
}

fn ensure_backfill_complete_at(home: &Path) -> io::Result<()> {
    loop {
        let connection = open_index(home)?;
        if backfill_complete(&connection)? {
            return Ok(());
        }
        drop(connection);

        let backfills = BACKFILLS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut active = backfills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.insert(home.to_path_buf()) {
            drop(active);
            let result = backfill(home);
            backfills
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(home);
            return result;
        }
        drop(active);
        thread::sleep(Duration::from_millis(25));
    }
}

fn open_index(home: &Path) -> io::Result<Connection> {
    fs::create_dir_all(home)?;
    let path = home.join(DATABASE_FILENAME);
    match open_and_initialize(&path) {
        Ok(connection) => Ok(connection),
        Err(error) if is_corrupt_database(&error) => {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
            let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
            open_and_initialize(&path).map_err(io::Error::other)
        }
        Err(error) => Err(io::Error::other(error)),
    }
}

fn is_corrupt_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            )
    )
}

fn open_and_initialize(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS index_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
    )?;
    let schema_version = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match schema_version.as_deref() {
        Some("4") => {}
        Some(_) => {
            connection.execute_batch(
                "DROP TABLE IF EXISTS sessions;
                     DELETE FROM index_meta;",
            )?;
            write_schema_version(&connection)?;
        }
        None => write_schema_version(&connection)?,
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
                 path TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 archived INTEGER NOT NULL,
                 title TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 summary_json TEXT NOT NULL,
                 health TEXT NOT NULL DEFAULT 'healthy',
                 health_issue_json TEXT,
                 source_fingerprint TEXT,
                 storage_identity TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS sessions_recency
                 ON sessions(archived, updated_at_ms DESC, created_at_ms DESC, session_id DESC);
             CREATE INDEX IF NOT EXISTS sessions_session_id
                 ON sessions(session_id, archived, updated_at_ms DESC);",
    )?;
    Ok(connection)
}

fn write_schema_version(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

fn seed_recent_if_empty(
    connection: &mut Connection,
    home: &Path,
    include_archived: bool,
) -> io::Result<()> {
    let count = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(io::Error::other)?;
    if count > 0 {
        return Ok(());
    }

    let mut candidates = Vec::new();
    collect_recent_by_path_date(
        &home.join("sessions"),
        false,
        RECENT_SEED_LIMIT,
        &mut candidates,
    );
    if include_archived && candidates.len() < RECENT_SEED_LIMIT {
        collect_recent_by_path_date(
            &home.join("archive"),
            true,
            RECENT_SEED_LIMIT - candidates.len(),
            &mut candidates,
        );
    }
    let summaries = candidates
        .into_iter()
        .filter_map(|(path, archived)| summarize_session_with_archive_flag(&path, archived).ok())
        .collect::<Vec<_>>();
    let transaction = connection.transaction().map_err(io::Error::other)?;
    for summary in summaries {
        upsert_summary_with_connection(&transaction, &summary)?;
    }
    transaction.commit().map_err(io::Error::other)
}

fn spawn_backfill_if_needed(home: PathBuf) {
    let backfills = BACKFILLS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut active = backfills
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if active.contains(&home) {
        return;
    }
    let complete = open_index(&home)
        .and_then(|connection| backfill_complete(&connection))
        .unwrap_or(false);
    if complete {
        return;
    }
    active.insert(home.clone());
    drop(active);

    let _ = thread::Builder::new()
        .name("orca-session-index-backfill".to_string())
        .spawn(move || {
            let _ = backfill(&home);
            BACKFILLS
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&home);
        });
}

fn backfill(home: &Path) -> io::Result<()> {
    let mut connection = open_index(home)?;
    let mut candidates = Vec::new();
    let sessions = home.join("sessions");
    if sessions.exists() {
        collect_session_files(&sessions, &mut |path| {
            candidates.push((path.to_path_buf(), false));
        })?;
    }
    let archive = home.join("archive");
    if archive.exists() {
        collect_session_files(&archive, &mut |path| {
            candidates.push((path.to_path_buf(), true));
        })?;
    }

    let summaries = candidates
        .into_iter()
        .filter_map(|(path, archived)| summarize_session_with_archive_flag(&path, archived).ok())
        .collect::<Vec<_>>();
    let transaction = connection.transaction().map_err(io::Error::other)?;
    for summary in summaries {
        upsert_summary_with_connection(&transaction, &summary)?;
    }
    transaction
        .execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES('backfill_complete', '1')",
            [],
        )
        .map_err(io::Error::other)?;
    transaction.commit().map_err(io::Error::other)
}

fn backfill_complete(connection: &Connection) -> io::Result<bool> {
    connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'backfill_complete'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map(|value| value.as_deref() == Some("1"))
        .map_err(io::Error::other)
}

fn upsert_summary_with_connection(
    connection: &Connection,
    summary: &SessionSummary,
) -> io::Result<()> {
    let summary_json = serde_json::to_string(summary).map_err(io::Error::other)?;
    connection
        .execute(
            "INSERT INTO sessions(
                 path, session_id, archived, title, created_at_ms, updated_at_ms, summary_json,
                 health, health_issue_json, source_fingerprint, storage_identity
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(path) DO UPDATE SET
                 session_id = excluded.session_id,
                 archived = excluded.archived,
                 title = excluded.title,
                 created_at_ms = excluded.created_at_ms,
                 updated_at_ms = excluded.updated_at_ms,
                 summary_json = excluded.summary_json,
                 health = excluded.health,
                 health_issue_json = excluded.health_issue_json,
                 source_fingerprint = excluded.source_fingerprint,
                 storage_identity = excluded.storage_identity",
            params![
                summary.path.to_string_lossy(),
                summary.session_id,
                summary.archived,
                summary.title,
                summary.created_at.timestamp_millis(),
                summary.updated_at.timestamp_millis(),
                summary_json,
                summary.health.as_str(),
                summary
                    .health_issue
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(io::Error::other)?,
                summary.source_fingerprint,
                summary.storage_identity
            ],
        )
        .map_err(io::Error::other)?;
    Ok(())
}

fn remove_path_with_connection(connection: &Connection, path: &Path) -> io::Result<()> {
    connection
        .execute(
            "DELETE FROM sessions WHERE path = ?1",
            [path.to_string_lossy()],
        )
        .map_err(io::Error::other)?;
    Ok(())
}

fn summary_from_meta(path: &Path, meta: SessionMeta, archived: bool) -> io::Result<SessionSummary> {
    let updated_at = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or(meta.created_at);
    let storage_identity = storage_identity_for_path(path);
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
        runtime_workspace_roots: meta.runtime_workspace_roots,
        permission_rule_count: meta.permission_rules.rules.len(),
        additional_working_directories: meta.additional_working_directories,
        network_domain_permissions: meta.network_domain_permissions,
        health: StoredSessionHealth::Healthy,
        health_issue: None,
        source_fingerprint: None,
        storage_identity,
    })
}

fn unreadable_summary(path: &Path, archived: bool, updated_at_ms: i64) -> SessionSummary {
    let updated_at = DateTime::<Utc>::from_timestamp_millis(updated_at_ms).unwrap_or_else(Utc::now);
    let storage_identity = storage_identity_for_path(path);
    SessionSummary {
        session_id: storage_identity.clone(),
        title: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Unreadable session")
            .to_string(),
        cwd: String::new(),
        provider: "unknown".to_string(),
        model: None,
        created_at: updated_at,
        updated_at,
        path: path.to_path_buf(),
        archived,
        parent_id: None,
        forked: false,
        approval_mode: None,
        active_permission_profile: None,
        runtime_workspace_roots: Vec::new(),
        permission_rule_count: 0,
        additional_working_directories: Vec::new(),
        network_domain_permissions: HashMap::new(),
        health: StoredSessionHealth::Quarantined,
        health_issue: Some(StoredSessionHealthIssue {
            code: "scan_failed".to_string(),
            line: None,
            offset: None,
        }),
        source_fingerprint: None,
        storage_identity,
    }
}

fn modified_millis(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(system_time_millis)
}

fn system_time_millis(time: SystemTime) -> Option<i64> {
    let millis = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(millis).ok()
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

fn collect_recent_by_path_date(
    root: &Path,
    archived: bool,
    limit: usize,
    out: &mut Vec<(PathBuf, bool)>,
) {
    if !root.exists() || limit == 0 {
        return;
    }
    let years = sorted_subdirs(root);
    if years.is_empty() {
        let _ = collect_session_files(root, &mut |path| {
            if out.len() < limit {
                out.push((path.to_path_buf(), archived));
            }
        });
        return;
    }
    for year in years {
        for month in sorted_subdirs(&year) {
            for day in sorted_subdirs(&month) {
                let _ = collect_leaf_files(&day, archived, limit, out);
                if out.len() >= limit {
                    return;
                }
            }
        }
    }
}

fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut directories = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_dir())
                .map(|_| entry.path())
        })
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| right.cmp(left));
    directories
}

fn collect_leaf_files(
    dir: &Path,
    archived: bool,
    limit: usize,
    out: &mut Vec<(PathBuf, bool)>,
) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .map(|file_type| (entry.path(), file_type))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| right.cmp(left));
    for (path, file_type) in entries {
        if out.len() >= limit {
            break;
        }
        if file_type.is_dir() {
            collect_leaf_files(&path, archived, limit, out)?;
        } else if file_type.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        {
            out.push((path, archived));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history;
    use crate::thread_store::types::SessionRecord;

    #[test]
    fn index_pages_searches_and_tracks_session_lifecycle() {
        let home = tempfile::tempdir().unwrap();
        let connection = open_index(home.path()).unwrap();
        for index in 0..45 {
            let summary = write_legacy_session(home.path(), index);
            upsert_summary_with_connection(&connection, &summary).unwrap();
        }
        connection
            .execute(
                "INSERT OR REPLACE INTO index_meta(key, value)
                 VALUES('backfill_complete', '1')",
                [],
            )
            .unwrap();

        let first = list_page_at(home.path(), 0, 20, false, None).unwrap();
        assert_eq!(first.sessions.len(), 20);
        assert_eq!(first.next_offset, Some(20));
        let second = list_page_at(home.path(), 20, 20, false, None).unwrap();
        assert_eq!(second.sessions.len(), 20);
        assert_eq!(second.next_offset, Some(40));
        let third = list_page_at(home.path(), 40, 20, false, None).unwrap();
        assert_eq!(third.sessions.len(), 5);
        assert_eq!(third.next_offset, None);

        let target = first.sessions[0].session_id.clone();
        let mut renamed = first.sessions[0].clone();
        renamed.title = "unique searchable title".to_string();
        renamed.updated_at += chrono::Duration::seconds(1);
        upsert_summary_with_connection(&connection, &renamed).unwrap();
        let search = list_page_at(home.path(), 0, 20, false, Some("searchable")).unwrap();
        assert_eq!(search.sessions.len(), 1);
        assert_eq!(search.sessions[0].session_id, target);

        let archived_path = home.path().join("archive").join(
            renamed
                .path
                .strip_prefix(home.path().join("sessions"))
                .unwrap(),
        );
        fs::create_dir_all(archived_path.parent().unwrap()).unwrap();
        fs::rename(&renamed.path, &archived_path).unwrap();
        move_path(&renamed.path, &archived_path, true).unwrap();
        assert!(
            list_page_at(home.path(), 0, 100, false, None)
                .unwrap()
                .sessions
                .iter()
                .all(|session| session.session_id != target)
        );
        assert!(
            list_page_at(home.path(), 0, 100, true, None)
                .unwrap()
                .sessions
                .iter()
                .any(|session| session.session_id == target && session.archived)
        );

        fs::remove_file(&archived_path).unwrap();
        remove_path(&archived_path).unwrap();
        assert!(
            list_page_at(home.path(), 0, 100, true, None)
                .unwrap()
                .sessions
                .iter()
                .all(|session| session.session_id != target)
        );
    }

    #[test]
    fn corrupt_index_rebuilds_and_legacy_sessions_backfill() {
        let home = tempfile::tempdir().unwrap();

        fs::write(
            home.path().join(DATABASE_FILENAME),
            b"not a sqlite database",
        )
        .unwrap();
        for index in 0..25 {
            write_legacy_session(home.path(), index);
        }

        let first = list_page_at(home.path(), 0, 20, false, None).unwrap();
        assert_eq!(first.sessions.len(), 20);
        ensure_backfill_complete_at(home.path()).unwrap();
        let second = list_page_at(home.path(), 20, 20, false, None).unwrap();
        assert_eq!(second.sessions.len(), 5);
        assert!(second.backfill_complete);
    }

    #[test]
    fn malformed_session_remains_visible_as_a_quarantined_catalog_row() {
        let home = tempfile::tempdir().unwrap();
        let path = home
            .path()
            .join("sessions")
            .join("2026")
            .join("01")
            .join("01")
            .join("unreadable.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = b"{\"type\":\"session.meta\",bad}\n";
        fs::write(&path, source).unwrap();

        let page = list_page_at(home.path(), 0, 20, false, None).unwrap();
        assert_eq!(page.sessions.len(), 1);
        let summary = &page.sessions[0];
        assert_eq!(summary.health, StoredSessionHealth::Quarantined);
        assert_eq!(summary.path, path);
        assert!(summary.session_id.starts_with("storage-"));
        assert_eq!(summary.session_id, summary.storage_identity);
        assert_eq!(fs::read(&summary.path).unwrap(), source);

        let connection = open_index(home.path()).unwrap();
        let health: String = connection
            .query_row(
                "SELECT health FROM sessions WHERE path = ?1",
                [path.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(health, "quarantined");
    }

    #[test]
    fn duplicate_metadata_ids_do_not_hide_each_other_in_the_catalog() {
        let home = tempfile::tempdir().unwrap();
        let first = write_legacy_session(home.path(), 1);
        let second_path = home
            .path()
            .join("sessions")
            .join("2026")
            .join("01")
            .join("02")
            .join("duplicate.jsonl");
        fs::create_dir_all(second_path.parent().unwrap()).unwrap();
        let mut second_meta = history::create_meta(home.path(), "mock", None, "duplicate id");
        second_meta.session_id = first.session_id.clone();
        fs::write(
            &second_path,
            format!(
                "{}\n",
                serde_json::to_string(&SessionRecord::Meta(second_meta)).unwrap()
            ),
        )
        .unwrap();
        let second = summarize_session_with_archive_flag(&second_path, false).unwrap();

        let connection = open_index(home.path()).unwrap();
        upsert_summary_with_connection(&connection, &first).unwrap();
        upsert_summary_with_connection(&connection, &second).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO index_meta(key, value)
                 VALUES('backfill_complete', '1')",
                [],
            )
            .unwrap();

        let page = list_page_at(home.path(), 0, 20, false, None).unwrap();
        assert_eq!(
            page.sessions
                .iter()
                .filter(|summary| summary.session_id == first.session_id)
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn indexed_path_replaced_by_symlink_is_evicted() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let summary = write_legacy_session(home.path(), 0);
        let connection = open_index(home.path()).unwrap();
        upsert_summary_with_connection(&connection, &summary).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO index_meta(key, value)
                 VALUES('backfill_complete', '1')",
                [],
            )
            .unwrap();

        let replacement = home.path().join("replacement.jsonl");
        fs::rename(&summary.path, &replacement).unwrap();
        symlink(&replacement, &summary.path).unwrap();

        let page = list_page_at(home.path(), 0, 20, false, None).unwrap();
        assert!(page.sessions.is_empty());
        let indexed_count = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(indexed_count, 0);
    }

    fn write_legacy_session(home: &Path, index: usize) -> SessionSummary {
        let mut meta = history::create_meta(
            home,
            "mock",
            Some("model".to_string()),
            &format!("indexed session {index:02}"),
        );
        meta.session_id = format!("session-{index:02}");
        meta.created_at += chrono::Duration::milliseconds(index as i64);
        let path = home
            .join("sessions")
            .join("2026")
            .join("01")
            .join("01")
            .join(format!("session-{index:02}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&SessionRecord::Meta(meta)).unwrap()
            ),
        )
        .unwrap();
        let mut summary = summarize_session_with_archive_flag(&path, false).unwrap();
        summary.updated_at = summary.created_at;
        summary
    }
}
