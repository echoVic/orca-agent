use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use orca_platform::fs::{ExclusiveFileLock, open_nofollow_nonblocking};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use super::{TaskOutputBuffer, TaskOutputChunk, TaskOutputRead, TaskOutputStream};

pub(super) const MAX_PAGE_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 8192;
const DATABASE: &str = "archive.sqlite3";

#[derive(Clone, Copy, Debug)]
pub(super) struct ArchiveLimits {
    pub task_bytes: usize,
    pub session_bytes: usize,
    pub tasks: usize,
    pub chunks: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            task_bytes: 32 * 1024 * 1024,
            session_bytes: 128 * 1024 * 1024,
            tasks: 256,
            chunks: 32 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ArchivedShell {
    pub task_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub requested_pty: bool,
    pub effective_pty: bool,
    pub cursor: usize,
}

pub(super) struct OutputArchive {
    connection: Connection,
    root: PathBuf,
    #[cfg(unix)]
    directory: File,
    #[cfg(unix)]
    database: File,
    #[cfg(windows)]
    _database: File,
    _owner: ExclusiveFileLock,
    limits: ArchiveLimits,
    write_failure: Option<String>,
}

type SharedArchive = Arc<Mutex<OutputArchive>>;
type ArchiveCache = Mutex<HashMap<PathBuf, Weak<Mutex<OutputArchive>>>>;

impl std::fmt::Debug for OutputArchive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputArchive")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl OutputArchive {
    pub(super) fn open(root: &Path, session_id: &str) -> io::Result<SharedArchive> {
        static OPEN_ARCHIVES: OnceLock<ArchiveCache> = OnceLock::new();
        // Same-process managers for bash and exec share the owner. A second host
        // cannot take over a live archive and mistake its processes for survivors.
        let mut cache = OPEN_ARCHIVES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(poisoned)?;
        cache.retain(|_, archive| archive.strong_count() != 0);
        prepare_directory(root)
            .map_err(|error| io_context("prepare task output archive directory", error))?;
        let root = fs::canonicalize(root)
            .map_err(|error| io_context("canonicalize task output archive directory", error))?;
        if let Some(archive) = cache.get(&root).and_then(Weak::upgrade) {
            archive
                .lock()
                .map_err(poisoned)?
                .verify_session(session_id)
                .map_err(|error| io_context("verify cached task output archive", error))?;
            return Ok(archive);
        }
        let archive = Arc::new(Mutex::new(Self::open_exclusive(
            &root,
            session_id,
            ArchiveLimits::default(),
        )?));
        cache.insert(root, Arc::downgrade(&archive));
        Ok(archive)
    }

    fn open_exclusive(root: &Path, session_id: &str, limits: ArchiveLimits) -> io::Result<Self> {
        if limits.task_bytes == 0
            || limits.session_bytes == 0
            || limits.tasks == 0
            || limits.chunks == 0
        {
            return Err(invalid("archive limits must be positive"));
        }
        prepare_directory(root)
            .map_err(|error| io_context("prepare task output archive directory", error))?;
        let root = fs::canonicalize(root)
            .map_err(|error| io_context("canonicalize task output archive directory", error))?;
        #[cfg(unix)]
        let directory = open_directory(&root)
            .map_err(|error| io_context("open task output archive directory", error))?;
        let lock_path = root.join("owner.lock");
        let owner_file = private_file(&lock_path)
            .map_err(|error| io_context("open task output archive owner file", error))?;
        let owner = ExclusiveFileLock::try_acquire_file(&lock_path, owner_file)
            .map_err(|error| io::Error::other(format!("task output archive owner: {error}")))?;
        let path = root.join(DATABASE);
        let database = private_file(&path)
            .map_err(|error| io_context("open task output archive database", error))?;
        check_sidecars(&root)
            .map_err(|error| io_context("verify task output archive sidecars", error))?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&path, flags).map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(sql_error)?;
        // DELETE journaling bounds sidecars by the database size. FULL sync
        // commits each observed chunk before it can be evicted from RAM.
        connection
            .execute_batch(
                "PRAGMA journal_mode = DELETE;
             PRAGMA synchronous = FULL;
             PRAGMA auto_vacuum = FULL;
             PRAGMA cache_size = -2048;
             PRAGMA mmap_size = 0;
             PRAGMA temp_store = MEMORY;
             PRAGMA foreign_keys = ON;
             PRAGMA trusted_schema = OFF;",
            )
            .map_err(sql_error)?;
        let max_pages = (limits.session_bytes.saturating_mul(2)
            + limits.chunks.saturating_mul(512)
            + limits.tasks.saturating_mul(4096)
            + 1024 * 1024)
            / 4096;
        connection
            .pragma_update(None, "max_page_count", max_pages)
            .map_err(sql_error)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS archive_meta (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                version INTEGER NOT NULL,
                session_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS shells (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                shell_id TEXT NOT NULL UNIQUE,
                task_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL,
                exit_code INTEGER,
                requested_pty INTEGER NOT NULL,
                effective_pty INTEGER NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0,
                total INTEGER NOT NULL DEFAULT 0,
                stdout_total INTEGER NOT NULL DEFAULT 0,
                stderr_total INTEGER NOT NULL DEFAULT 0,
                retained_start INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS chunks (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES shells(task_id) ON DELETE CASCADE,
                start INTEGER NOT NULL,
                end INTEGER NOT NULL,
                stream INTEGER NOT NULL,
                stdout_before INTEGER NOT NULL,
                stderr_before INTEGER NOT NULL,
                content BLOB NOT NULL,
                UNIQUE(task_id, start)
             );
             CREATE TABLE IF NOT EXISTS usage (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                bytes INTEGER NOT NULL,
                chunks INTEGER NOT NULL
             );
             INSERT OR IGNORE INTO usage VALUES(1, 0, 0);
             CREATE TRIGGER IF NOT EXISTS chunk_insert AFTER INSERT ON chunks BEGIN
                UPDATE usage SET bytes = bytes + NEW.end - NEW.start, chunks = chunks + 1 WHERE id = 1;
             END;
             CREATE TRIGGER IF NOT EXISTS chunk_delete AFTER DELETE ON chunks BEGIN
                UPDATE usage SET bytes = bytes - OLD.end + OLD.start, chunks = chunks - 1 WHERE id = 1;
                UPDATE shells SET retained_start = MAX(retained_start, OLD.end) WHERE task_id = OLD.task_id;
             END;",
        ).map_err(sql_error)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO archive_meta VALUES(1, 1, ?1)",
                [session_id],
            )
            .map_err(sql_error)?;
        let archive = Self {
            connection,
            root,
            #[cfg(unix)]
            directory,
            #[cfg(unix)]
            database,
            #[cfg(windows)]
            _database: database,
            _owner: owner,
            limits,
            write_failure: None,
        };
        archive
            .verify_session(session_id)
            .map_err(|error| io_context("initialize task output archive", error))?;
        archive
            .connection
            .execute(
                "UPDATE shells SET state = 'interrupted', exit_code = NULL WHERE state = 'running'",
                [],
            )
            .map_err(sql_error)?;
        Ok(archive)
    }

    fn verify_session(&self, session_id: &str) -> io::Result<()> {
        self.check_files()?;
        let (version, owner): (i64, String) = self
            .connection
            .query_row(
                "SELECT version, session_id FROM archive_meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(sql_error)?;
        if version != 1 || owner != session_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task output archive scope/version mismatch",
            ));
        }
        Ok(())
    }

    fn check_files(&self) -> io::Result<()> {
        if let Some(error) = &self.write_failure {
            return Err(io::Error::other(format!(
                "task output archive unavailable after write failure: {error}"
            )));
        }
        check_components(&self.root)
            .map_err(|error| io_context("validate task output archive path", error))?;
        #[cfg(unix)]
        {
            verify_identity(&self.root, &self.directory, true)
                .map_err(|error| io_context("verify task output archive directory", error))?;
            verify_identity(&self.root.join(DATABASE), &self.database, false)
                .map_err(|error| io_context("verify task output archive database", error))?;
            verify_identity(&self.root.join("owner.lock"), self._owner.file(), false)
                .map_err(|error| io_context("verify task output archive owner file", error))?;
        }
        #[cfg(windows)]
        {
            verify_named_path(&self.root.join(DATABASE), false)
                .map_err(|error| io_context("verify task output archive database", error))?;
            verify_named_path(&self.root.join("owner.lock"), false)
                .map_err(|error| io_context("verify task output archive owner file", error))?;
        }
        check_sidecars(&self.root)
            .map_err(|error| io_context("verify task output archive sidecars", error))
    }

    pub(super) fn register(
        &mut self,
        shell_id: &str,
        task_id: &str,
        requested_pty: bool,
        effective_pty: bool,
    ) -> io::Result<()> {
        validate_id(shell_id)?;
        validate_id(task_id)?;
        self.check_files()?;
        let tx = self.connection.transaction().map_err(sql_error)?;
        let count: usize = tx
            .query_row("SELECT COUNT(*) FROM shells", [], |row| row.get(0))
            .map_err(sql_error)?;
        if count >= self.limits.tasks {
            let removed = tx
                .execute(
                    "DELETE FROM shells WHERE sequence = (
                    SELECT sequence FROM shells WHERE state != 'running' ORDER BY sequence LIMIT 1
                 )",
                    [],
                )
                .map_err(sql_error)?;
            if removed == 0 {
                return Err(io::Error::other(
                    "task output archive has too many active tasks",
                ));
            }
        }
        tx.execute(
            "INSERT INTO shells(shell_id, task_id, state, requested_pty, effective_pty)
             VALUES(?1, ?2, 'running', ?3, ?4)",
            params![shell_id, task_id, requested_pty, effective_pty],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)
    }

    pub(super) fn shell(&self, shell_id: &str) -> io::Result<ArchivedShell> {
        validate_id(shell_id)?;
        self.check_files()?;
        self.connection
            .query_row(
                "SELECT task_id, state, exit_code, requested_pty, effective_pty, cursor
             FROM shells WHERE shell_id = ?1",
                [shell_id],
                |row| {
                    Ok(ArchivedShell {
                        task_id: row.get(0)?,
                        state: row.get(1)?,
                        exit_code: row.get(2)?,
                        requested_pty: row.get(3)?,
                        effective_pty: row.get(4)?,
                        cursor: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(missing)
    }

    pub(super) fn finish(
        &self,
        task_id: &str,
        state: &str,
        exit_code: Option<i32>,
    ) -> io::Result<()> {
        self.check_files()?;
        if !matches!(state, "exited" | "cancelled" | "timed_out" | "interrupted") {
            return Err(invalid("invalid task output terminal state"));
        }
        if self
            .connection
            .execute(
                "UPDATE shells SET state = ?2, exit_code = ?3 WHERE task_id = ?1",
                params![task_id, state, exit_code],
            )
            .map_err(sql_error)?
            != 1
        {
            return Err(missing());
        }
        Ok(())
    }

    pub(super) fn set_cursor(&self, shell_id: &str, cursor: usize) -> io::Result<()> {
        self.check_files()?;
        if self
            .connection
            .execute(
                "UPDATE shells SET cursor = ?2 WHERE shell_id = ?1 AND total >= ?2",
                params![shell_id, offset(cursor)?],
            )
            .map_err(sql_error)?
            != 1
        {
            return Err(invalid("invalid automatic output cursor"));
        }
        Ok(())
    }

    pub(super) fn append(
        &mut self,
        task_id: &str,
        stream: TaskOutputStream,
        content: &str,
    ) -> io::Result<()> {
        self.check_files()?;
        // Limit individual SQLite allocations even for a caller-provided giant append.
        let mut remaining = content;
        while !remaining.is_empty() {
            let end = super::utf8_ceil(remaining, CHUNK_BYTES.min(remaining.len()));
            if let Err(error) = self.append_chunk(task_id, stream, &remaining[..end]) {
                self.write_failure = Some(error.to_string());
                return Err(error);
            }
            remaining = &remaining[end..];
        }
        Ok(())
    }

    fn append_chunk(
        &mut self,
        task_id: &str,
        stream: TaskOutputStream,
        content: &str,
    ) -> io::Result<()> {
        let tx = self.connection.transaction().map_err(sql_error)?;
        let (start, stdout, stderr): (i64, i64, i64) = tx.query_row(
            "SELECT total, stdout_total, stderr_total FROM shells WHERE task_id = ?1 AND state = 'running'",
            [task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(sql_error)?;
        let end = start
            .checked_add(offset(content.len())?)
            .ok_or_else(|| invalid("output offset overflow"))?;
        let stream_id = if stream == TaskOutputStream::Stdout {
            0
        } else {
            1
        };
        tx.execute(
            "INSERT INTO chunks(task_id, start, end, stream, stdout_before, stderr_before, content)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task_id,
                start,
                end,
                stream_id,
                stdout,
                stderr,
                content.as_bytes()
            ],
        )
        .map_err(sql_error)?;
        tx.execute(
            "UPDATE shells SET total = ?2, stdout_total = ?3, stderr_total = ?4 WHERE task_id = ?1",
            params![
                task_id,
                end,
                stdout
                    + if stream_id == 0 {
                        offset(content.len())?
                    } else {
                        0
                    },
                stderr
                    + if stream_id == 1 {
                        offset(content.len())?
                    } else {
                        0
                    }
            ],
        )
        .map_err(sql_error)?;
        let cutoff = end.saturating_sub(offset(
            self.limits.task_bytes.min(self.limits.session_bytes),
        )?);
        tx.execute(
            "DELETE FROM chunks WHERE task_id = ?1 AND end <= ?2",
            params![task_id, cutoff],
        )
        .map_err(sql_error)?;
        let first: Option<(i64, i64, Vec<u8>)> = tx
            .query_row(
                "SELECT start, stream, content FROM chunks
             WHERE task_id = ?1 AND start < ?2 ORDER BY start LIMIT 1",
                params![task_id, cutoff],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql_error)?;
        if let Some((start, stream, bytes)) = first {
            let content = std::str::from_utf8(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let trim = super::utf8_ceil(content, (cutoff - start) as usize);
            if trim == bytes.len() {
                tx.execute(
                    "DELETE FROM chunks WHERE task_id = ?1 AND start = ?2",
                    params![task_id, start],
                )
                .map_err(sql_error)?;
            } else {
                tx.execute(
                    "UPDATE chunks SET start = start + ?3, content = ?4,
                     stdout_before = stdout_before + ?5, stderr_before = stderr_before + ?6
                     WHERE task_id = ?1 AND start = ?2",
                    params![
                        task_id,
                        start,
                        offset(trim)?,
                        &bytes[trim..],
                        if stream == 0 { offset(trim)? } else { 0 },
                        if stream == 1 { offset(trim)? } else { 0 }
                    ],
                )
                .map_err(sql_error)?;
                tx.execute(
                    "UPDATE usage SET bytes = bytes - ?1 WHERE id = 1",
                    [offset(trim)?],
                )
                .map_err(sql_error)?;
                tx.execute(
                    "UPDATE shells SET retained_start = ?2 WHERE task_id = ?1",
                    params![task_id, start + offset(trim)?],
                )
                .map_err(sql_error)?;
            }
        }
        loop {
            let (bytes, chunks): (usize, usize) = tx
                .query_row("SELECT bytes, chunks FROM usage WHERE id = 1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .map_err(sql_error)?;
            if bytes <= self.limits.session_bytes && chunks <= self.limits.chunks {
                break;
            }
            let removed = tx
                .execute(
                    "DELETE FROM chunks WHERE sequence = (SELECT MIN(sequence) FROM chunks)",
                    [],
                )
                .map_err(sql_error)?;
            if removed == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid task output usage metadata",
                ));
            }
        }
        tx.commit().map_err(sql_error)
    }

    pub(super) fn tail(&self, task_id: &str, max_bytes: usize) -> io::Result<TaskOutputRead> {
        let total = self.read(task_id, 0, 0)?.bytes_total;
        let from = total.saturating_sub(max_bytes.min(MAX_PAGE_BYTES));
        let page = self.read(task_id, from, max_bytes)?;
        let mut start = page.next_offset - page.combined.len();
        let chunk_start: Option<usize> = self.connection.query_row(
            "SELECT start FROM chunks WHERE task_id = ?1 AND start <= ?2 ORDER BY start DESC LIMIT 1",
            params![task_id, offset(from)?], |row| row.get(0),
        ).optional().map_err(sql_error)?;
        if chunk_start.is_some_and(|chunk_start| chunk_start < from)
            && page.combined.starts_with('\n')
        {
            start += 1;
        }
        let mut page = self.read(task_id, start, max_bytes)?;
        page.omitted_prefix_bytes = start;
        Ok(page)
    }

    pub(super) fn read(
        &self,
        task_id: &str,
        from: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        self.check_files()?;
        let (total, retained, stdout_total, stderr_total): (usize, usize, usize, usize) =
            self.connection.query_row(
                "SELECT total, retained_start, stdout_total, stderr_total FROM shells WHERE task_id = ?1",
                [task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(sql_error)?.ok_or_else(missing)?;
        if retained > total || stdout_total.checked_add(stderr_total) != Some(total) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid task output byte totals",
            ));
        }
        if from > total {
            return Err(invalid("output_offset exceeds output_bytes_total"));
        }
        let start = from.max(retained);
        let end = start
            .saturating_add(max_bytes.min(MAX_PAGE_BYTES))
            .min(total);
        let first: Option<usize> = self.connection.query_row(
            "SELECT start FROM chunks WHERE task_id = ?1 AND start <= ?2 ORDER BY start DESC LIMIT 1",
            params![task_id, offset(start)?], |row| row.get(0),
        ).optional().map_err(sql_error)?;
        let mut buffer = TaskOutputBuffer {
            chunks: Vec::new(),
            bytes_total: total,
            trimmed_stdout_bytes: stdout_total,
            trimmed_stderr_bytes: stderr_total,
        };
        if start < total {
            let mut stmt = self.connection.prepare(
                "SELECT start, stream, substr(content, 1, 8196), stdout_before, stderr_before, length(content) FROM chunks
                 WHERE task_id = ?1 AND start >= ?2 AND start < ?3 ORDER BY start",
            ).map_err(sql_error)?;
            let mut rows = stmt
                .query(params![
                    task_id,
                    offset(first.unwrap_or(start))?,
                    offset(end.max(start + 1))?
                ])
                .map_err(sql_error)?;
            let mut expected = first.unwrap_or(start);
            while let Some(row) = rows.next().map_err(sql_error)? {
                let chunk_start: usize = row.get(0).map_err(sql_error)?;
                let stream: i64 = row.get(1).map_err(sql_error)?;
                let bytes: Vec<u8> = row.get(2).map_err(sql_error)?;
                let stored_len: usize = row.get(5).map_err(sql_error)?;
                if chunk_start != expected || bytes.is_empty() || stored_len > CHUNK_BYTES + 3 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "task output archive has invalid chunk bounds",
                    ));
                }
                if buffer.chunks.is_empty() {
                    buffer.trimmed_stdout_bytes = row.get(3).map_err(sql_error)?;
                    buffer.trimmed_stderr_bytes = row.get(4).map_err(sql_error)?;
                }
                let content = String::from_utf8(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                expected = chunk_start + content.len();
                buffer.chunks.push(TaskOutputChunk {
                    start: chunk_start,
                    content,
                    stream: match stream {
                        0 => TaskOutputStream::Stdout,
                        1 => TaskOutputStream::Stderr,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid output stream",
                            ));
                        }
                    },
                });
            }
            if expected < end || expected <= start {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "task output archive is missing chunks",
                ));
            }
        }
        Ok(buffer.read_range(start, end, start - from))
    }
}

fn offset(value: usize) -> io::Result<i64> {
    i64::try_from(value).map_err(|_| invalid("output offset is too large"))
}

fn validate_id(value: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid("invalid shell/task id"));
    }
    Ok(())
}

fn prepare_directory(path: &Path) -> io::Result<()> {
    check_components(path)?;
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    check_components(path)?;
    check_private(&fs::symlink_metadata(path)?, true)
}

fn check_components(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(invalid("task output root must be absolute"));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(invalid("task output root cannot contain parent traversal"));
        }
        current.push(component);
        // A Windows drive prefix such as `D:` is drive-relative until the
        // following root component is appended, so it cannot be inspected yet.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => {
                // macOS exposes its system temp directories through these aliases.
                #[cfg(target_os = "macos")]
                if current == Path::new("/var") || current == Path::new("/tmp") {
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "task output path contains a symlink",
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(invalid("task output root is not a directory"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            check_private(&fs::symlink_metadata(path)?, false)?;
            open_nofollow_nonblocking(path).map_err(io::Error::other)?
        }
        Err(error) => return Err(error),
    };
    verify_identity(path, &file, false)?;
    Ok(file)
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path)
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn check_private(meta: &fs::Metadata, directory: bool) -> io::Result<()> {
    if is_link(meta) || (directory && !meta.is_dir()) || (!directory && !meta.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe task output artifact type",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
            || (!directory && meta.nlink() != 1)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task output artifacts must be private and owned by the current user",
            ));
        }
    }
    Ok(())
}

fn verify_identity(path: &Path, file: &File, directory: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    check_private(&meta, directory)?;
    let opened = file.metadata()?;
    check_private(&opened, directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.dev() != opened.dev() || meta.ino() != opened.ino() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task output artifact was replaced",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn verify_named_path(path: &Path, directory: bool) -> io::Result<()> {
    check_private(&fs::symlink_metadata(path)?, directory)
}

fn check_sidecars(root: &Path) -> io::Result<()> {
    for name in [
        "archive.sqlite3-journal",
        "archive.sqlite3-wal",
        "archive.sqlite3-shm",
    ] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(meta) => check_private(&meta, false)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn io_context(operation: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

fn missing() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "task output archive entry is missing or expired in this session",
    )
}

fn sql_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("task output archive unavailable: {error}"))
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("task output archive lock poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(root: &Path, limits: ArchiveLimits) -> OutputArchive {
        OutputArchive::open_exclusive(root, "test-session", limits).unwrap()
    }

    fn register(archive: &mut OutputArchive, suffix: &str) {
        archive
            .register(
                &format!("shell-{suffix}"),
                &format!("task-{suffix}"),
                false,
                false,
            )
            .unwrap();
    }

    #[test]
    fn archive_pages_match_memory_utf8_contract_and_replay() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = archive(&temp.path().join("output"), ArchiveLimits::default());
        register(&mut archive, "one");
        let empty = archive.read("task-one", 0, 10).unwrap();
        assert_eq!(empty.bytes_total, 0);
        assert_eq!(empty.next_offset, 0);
        assert_eq!(empty.combined, "");
        let memory = super::super::TaskOutputStore::new();
        for (stream, text) in [
            (TaskOutputStream::Stdout, "first\n"),
            (TaskOutputStream::Stderr, "\u{9519}\u{8bef}\n"),
            (TaskOutputStream::Stdout, "last\n"),
        ] {
            archive.append("task-one", stream, text).unwrap();
            memory.append("task-one", stream, text).unwrap();
        }
        for from in 0..=18 {
            for limit in 0..=20 {
                let page = archive.read("task-one", from, limit).unwrap();
                assert_eq!(
                    page,
                    memory.read_delta("task-one", from, limit).unwrap(),
                    "offset={from}, limit={limit}"
                );
                assert_eq!(archive.read("task-one", from, limit).unwrap(), page);
            }
        }
        for limit in 0..=20 {
            assert_eq!(
                archive.tail("task-one", limit).unwrap(),
                memory.tail("task-one", limit).unwrap()
            );
        }
        let first = archive.read("task-one", 0, 6).unwrap();
        let next = archive.read("task-one", first.next_offset, 7).unwrap();
        assert_eq!(first.combined, "first\n");
        assert_eq!(next.stderr, "\u{9519}\u{8bef}\n");
        assert_eq!(archive.read("task-one", 18, 10).unwrap().bytes_read, 0);
        for from in [19, usize::MAX] {
            assert_eq!(
                archive.read("task-one", from, 8).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            archive.read("missing", 0, 8).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            archive.shell("../shell-one").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn archive_caps_trim_utf8_and_keep_absolute_stream_prefixes() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = archive(
            &temp.path().join("output"),
            ArchiveLimits {
                task_bytes: 4,
                ..ArchiveLimits::default()
            },
        );
        register(&mut archive, "one");
        archive
            .append("task-one", TaskOutputStream::Stdout, "a\u{9519}\u{8bef}b")
            .unwrap();
        let page = archive.read("task-one", 0, 99).unwrap();
        assert_eq!(page.combined, "\u{8bef}b");
        assert_eq!(
            (
                page.omitted_prefix_bytes,
                page.stdout_prefix_bytes,
                page.bytes_total
            ),
            (4, 4, 8)
        );
        archive
            .append("task-one", TaskOutputStream::Stderr, "!!")
            .unwrap();
        let page = archive.read("task-one", 0, 99).unwrap();
        assert_eq!(page.combined, "b!!");
        assert_eq!(
            (
                page.omitted_prefix_bytes,
                page.stdout_prefix_bytes,
                page.stderr_prefix_bytes
            ),
            (7, 7, 0)
        );
        assert_eq!(page.next_offset, 10);
    }

    #[test]
    fn archive_bounds_session_bytes_chunk_rows_and_task_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let mut archive = archive(
            &root,
            ArchiveLimits {
                task_bytes: 12,
                session_bytes: 12,
                tasks: 2,
                chunks: 3,
            },
        );
        register(&mut archive, "one");
        archive
            .append("task-one", TaskOutputStream::Stdout, "12345678")
            .unwrap();
        register(&mut archive, "two");
        archive
            .append("task-two", TaskOutputStream::Stderr, "abcdefgh")
            .unwrap();
        let trimmed = archive.read("task-one", 0, 20).unwrap();
        assert_eq!(trimmed.combined, "");
        assert_eq!((trimmed.omitted_prefix_bytes, trimmed.bytes_total), (8, 8));
        assert!(
            archive
                .register("shell-three", "task-three", false, false)
                .is_err()
        );
        archive.finish("task-one", "exited", Some(0)).unwrap();
        register(&mut archive, "three");
        assert_eq!(
            archive.shell("shell-one").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for _ in 0..20 {
            archive
                .append("task-three", TaskOutputStream::Stdout, "x")
                .unwrap();
        }
        let (bytes, chunks): (usize, usize) = archive
            .connection
            .query_row("SELECT bytes, chunks FROM usage WHERE id = 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert!(bytes <= 12);
        assert!(chunks <= 3);
        assert_eq!(
            archive
                .read("task-three", 0, 100)
                .unwrap()
                .omitted_prefix_bytes,
            17
        );
        let count: usize = archive
            .connection
            .query_row("SELECT COUNT(*) FROM shells", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let size: u64 = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(size < 1024 * 1024, "small archive grew to {size} bytes");
    }

    #[test]
    fn archive_page_allocation_and_chunk_sizes_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = archive(&temp.path().join("output"), ArchiveLimits::default());
        register(&mut archive, "one");
        let text = format!(
            "{}\u{9519}{}",
            "a".repeat(CHUNK_BYTES - 1),
            "b".repeat(MAX_PAGE_BYTES)
        );
        archive
            .append("task-one", TaskOutputStream::Stdout, &text)
            .unwrap();
        let page = archive.read("task-one", 0, usize::MAX).unwrap();
        assert!(page.combined.len() <= MAX_PAGE_BYTES + 3);
        assert_eq!(&text[..page.next_offset], page.combined);
        let next = archive
            .read("task-one", page.next_offset, usize::MAX)
            .unwrap();
        assert_eq!(page.combined + &next.combined, text);
        let max_chunk: usize = archive
            .connection
            .query_row("SELECT MAX(length(content)) FROM chunks", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(max_chunk <= CHUNK_BYTES + 3);
    }

    #[test]
    fn archive_reopen_preserves_mapping_cursor_and_completed_status() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        {
            let mut archive = archive(&root, ArchiveLimits::default());
            archive
                .register("shell-one", "task-one", true, true)
                .unwrap();
            archive
                .append("task-one", TaskOutputStream::Stdout, "complete")
                .unwrap();
            archive.set_cursor("shell-one", 3).unwrap();
            archive.finish("task-one", "exited", Some(7)).unwrap();
        }
        let archive = archive(&root, ArchiveLimits::default());
        let shell = archive.shell("shell-one").unwrap();
        assert_eq!(
            (
                shell.task_id.as_str(),
                shell.state.as_str(),
                shell.exit_code,
                shell.cursor
            ),
            ("task-one", "exited", Some(7), 3)
        );
        assert!(shell.requested_pty && shell.effective_pty);
        assert_eq!(
            archive.read("task-one", shell.cursor, 99).unwrap().combined,
            "plete"
        );
        assert!(archive.verify_session("other-session").is_err());
    }

    #[test]
    fn archive_owner_lock_and_process_restart_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let run_child = |mode: &str| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "task_output::persistence::tests::archive_process_fixture",
                    "--nocapture",
                ])
                .env("ORCA_OUTPUT_TEST_ROOT", &root)
                .env("ORCA_OUTPUT_TEST_MODE", mode)
                .output()
                .unwrap()
        };
        {
            let _owner = archive(&root, ArchiveLimits::default());
            let result = run_child("locked");
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let result = run_child("write");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let archive = archive(&root, ArchiveLimits::default());
        let shell = archive.shell("shell-restart").unwrap();
        assert_eq!(shell.state, "interrupted");
        assert_eq!(shell.exit_code, None);
        assert_eq!(
            archive.read("task-restart", 0, 99).unwrap().combined,
            "durable"
        );
    }

    #[test]
    fn archive_process_fixture() {
        let Some(root) = std::env::var_os("ORCA_OUTPUT_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        if std::env::var("ORCA_OUTPUT_TEST_MODE").unwrap() == "locked" {
            assert!(
                OutputArchive::open_exclusive(&root, "test-session", ArchiveLimits::default())
                    .is_err()
            );
            return;
        }
        let mut archive = archive(&root, ArchiveLimits::default());
        register(&mut archive, "restart");
        archive
            .append("task-restart", TaskOutputStream::Stdout, "durable")
            .unwrap();
        // Exit without destructors, as when a runtime host disappears.
        std::process::exit(0);
    }

    #[test]
    fn archive_missing_database_is_not_an_empty_success() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        let mut archive = archive(&root, ArchiveLimits::default());
        register(&mut archive, "one");
        fs::remove_file(root.join(DATABASE)).unwrap();
        assert_eq!(
            archive.read("task-one", 0, 10).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn archive_rejects_corruption_and_non_file_database() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("output");
        {
            let _archive = archive(&root, ArchiveLimits::default());
        }
        fs::write(root.join(DATABASE), "not sqlite").unwrap();
        assert!(OutputArchive::open(&root, "test-session").is_err());
        fs::remove_file(root.join(DATABASE)).unwrap();
        fs::create_dir(root.join(DATABASE)).unwrap();
        assert!(OutputArchive::open(&root, "test-session").is_err());
    }

    #[test]
    fn archive_rejects_inconsistent_totals_and_missing_chunk_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = archive(&temp.path().join("output"), ArchiveLimits::default());
        register(&mut archive, "one");
        archive
            .append("task-one", TaskOutputStream::Stdout, "abc")
            .unwrap();
        archive
            .connection
            .execute("DELETE FROM chunks", [])
            .unwrap();
        archive
            .connection
            .execute("UPDATE shells SET retained_start = 0", [])
            .unwrap();
        for limit in [0, 10] {
            assert_eq!(
                archive.read("task-one", 0, limit).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        archive
            .connection
            .execute("UPDATE shells SET total = 2", [])
            .unwrap();
        assert_eq!(
            archive.read("task-one", 0, 10).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn archive_write_failure_blocks_successful_partial_replay() {
        let temp = tempfile::tempdir().unwrap();
        let mut archive = archive(&temp.path().join("output"), ArchiveLimits::default());
        register(&mut archive, "one");
        archive
            .append("task-one", TaskOutputStream::Stdout, "before")
            .unwrap();
        archive
            .connection
            .pragma_update(None, "query_only", true)
            .unwrap();
        assert!(
            archive
                .append("task-one", TaskOutputStream::Stdout, "lost")
                .is_err()
        );
        assert!(archive.read("task-one", 0, 99).is_err());
        assert!(archive.shell("shell-one").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn archive_rejects_links_nonprivate_paths_and_scope_collisions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = temp.path().join("linked");
        symlink(outside.path(), &link).unwrap();
        assert!(OutputArchive::open(&link, "one").is_err());
        let root = temp.path().join("root");
        {
            let mut archive = archive(&root, ArchiveLimits::default());
            register(&mut archive, "one");
            assert!(archive.verify_session("other").is_err());
            let metadata = fs::metadata(root.join(DATABASE)).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            symlink(
                outside.path().join("victim"),
                root.join("archive.sqlite3-journal"),
            )
            .unwrap();
            assert!(archive.read("task-one", 0, 10).is_err());
            fs::remove_file(root.join("archive.sqlite3-journal")).unwrap();
            fs::remove_file(root.join(DATABASE)).unwrap();
            symlink(outside.path().join("victim"), root.join(DATABASE)).unwrap();
            assert!(archive.read("task-one", 0, 10).is_err());
        }
        assert!(OutputArchive::open(&root, "test-session").is_err());
        fs::remove_file(root.join(DATABASE)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(OutputArchive::open(&root, "test-session").is_err());
        assert!(!outside.path().join("victim").exists());
    }
}
