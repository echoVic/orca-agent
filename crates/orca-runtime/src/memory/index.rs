use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};

const INDEX_SCHEMA_VERSION: &str = "1";
const INDEX_FILENAME: &str = "index.sqlite3";

pub(super) struct IndexRecord<'a> {
    pub(super) key: &'a str,
    pub(super) search_text: &'a str,
    pub(super) recorded_at_ms: i64,
}

pub(super) fn rebuild_best_effort(
    root: &Path,
    ledger_fingerprint: &str,
    records: &[IndexRecord<'_>],
) {
    if let Err(error) = rebuild(root, ledger_fingerprint, records) {
        eprintln!("orca: warning: automatic memory search index rebuild failed: {error}");
    }
}

pub(super) fn search(
    root: &Path,
    ledger_fingerprint: &str,
    records: &[IndexRecord<'_>],
    query_tokens: &[String],
) -> Result<Vec<String>, String> {
    if query_tokens.is_empty() {
        return Ok(Vec::new());
    }
    let path = index_path(root);
    let mut connection = open_or_rebuild(&path, ledger_fingerprint, records)?;
    let indexed_fingerprint = read_meta(&connection, "ledger_fingerprint")
        .map_err(|error| format!("failed to read memory index metadata: {error}"))?;
    if indexed_fingerprint.as_deref() != Some(ledger_fingerprint) {
        rebuild_in_connection(&mut connection, ledger_fingerprint, records)?;
    }

    let query = query_tokens
        .iter()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut statement = connection
        .prepare(
            "SELECT candidate_key, bm25(memory_fts) AS rank
             FROM memory_fts
             WHERE memory_fts MATCH ?1
             ORDER BY rank ASC, recorded_at_ms DESC",
        )
        .map_err(|error| format!("failed to prepare memory index query: {error}"))?;
    let ranked = statement
        .query_map([query], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })
        .map_err(|error| format!("failed to query memory index: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read memory index result: {error}"))?;
    Ok(ranked.into_iter().map(|(key, _)| key).collect())
}

fn rebuild(
    root: &Path,
    ledger_fingerprint: &str,
    records: &[IndexRecord<'_>],
) -> Result<(), String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("failed to create memory index directory: {error}"))?;
    let path = index_path(root);
    let mut connection = open_or_rebuild(&path, ledger_fingerprint, records)?;
    rebuild_in_connection(&mut connection, ledger_fingerprint, records)
}

fn open_or_rebuild(
    path: &Path,
    ledger_fingerprint: &str,
    records: &[IndexRecord<'_>],
) -> Result<Connection, String> {
    reject_symlink(path)?;
    match open_and_initialize(path) {
        Ok(connection) => Ok(connection),
        Err(error) if is_corrupt_database(&error) => {
            remove_derived_database(path)?;
            let mut connection = open_and_initialize(path)
                .map_err(|error| format!("failed to recreate memory index: {error}"))?;
            rebuild_in_connection(&mut connection, ledger_fingerprint, records)?;
            Ok(connection)
        }
        Err(error) => Err(format!("failed to open memory index: {error}")),
    }
}

fn open_and_initialize(path: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS index_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
             candidate_key UNINDEXED,
             search_text,
             recorded_at_ms UNINDEXED,
             tokenize = 'unicode61'
         );",
    )?;
    let integrity =
        connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))?;
    if integrity != "ok" {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(integrity),
        ));
    }
    let schema_version = read_meta(&connection, "schema_version")?;
    if schema_version.as_deref() != Some(INDEX_SCHEMA_VERSION) {
        connection.execute_batch(
            "DROP TABLE IF EXISTS memory_fts;
             DELETE FROM index_meta;
             CREATE VIRTUAL TABLE memory_fts USING fts5(
                 candidate_key UNINDEXED,
                 search_text,
                 recorded_at_ms UNINDEXED,
                 tokenize = 'unicode61'
             );",
        )?;
        connection.execute(
            "INSERT INTO index_meta(key, value) VALUES('schema_version', ?1)",
            [INDEX_SCHEMA_VERSION],
        )?;
    }
    Ok(connection)
}

fn rebuild_in_connection(
    connection: &mut Connection,
    ledger_fingerprint: &str,
    records: &[IndexRecord<'_>],
) -> Result<(), String> {
    let transaction = connection
        .transaction()
        .map_err(|error| format!("failed to start memory index rebuild: {error}"))?;
    transaction
        .execute("DELETE FROM memory_fts", [])
        .map_err(|error| format!("failed to clear memory index: {error}"))?;
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO memory_fts(candidate_key, search_text, recorded_at_ms)
                 VALUES(?1, ?2, ?3)",
            )
            .map_err(|error| format!("failed to prepare memory index rebuild: {error}"))?;
        for record in records {
            insert
                .execute(params![
                    record.key,
                    record.search_text,
                    record.recorded_at_ms
                ])
                .map_err(|error| format!("failed to insert memory index record: {error}"))?;
        }
    }
    transaction
        .execute(
            "INSERT OR REPLACE INTO index_meta(key, value)
             VALUES('ledger_fingerprint', ?1)",
            [ledger_fingerprint],
        )
        .map_err(|error| format!("failed to update memory index metadata: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("failed to commit memory index rebuild: {error}"))
}

fn read_meta(connection: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "refusing to follow memory index symlink: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to inspect memory index: {error}")),
    }
}

fn is_corrupt_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(inner.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn remove_derived_database(path: &Path) -> Result<(), String> {
    reject_symlink(path)?;
    for candidate in [
        path.to_path_buf(),
        sidecar_path(path, "-wal"),
        sidecar_path(path, "-shm"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to remove corrupt derived memory index {}: {error}",
                    candidate.display()
                ));
            }
        }
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn index_path(root: &Path) -> PathBuf {
    // SQLITE_OPEN_NOFOLLOW also rejects symlinked parent components. Resolve
    // the already-created directory first (not the database file itself), then
    // keep NOFOLLOW on the final index path.
    fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .join(INDEX_FILENAME)
}
