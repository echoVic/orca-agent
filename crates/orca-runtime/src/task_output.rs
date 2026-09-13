use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

mod persistence;
pub(crate) use persistence::ArchivedShell;
use persistence::OutputArchive;

pub const DEFAULT_TASK_OUTPUT_RETAINED_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const TASK_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RETAINED_CHUNKS: usize = 32 * 1024;

#[derive(Clone, Debug)]
pub struct TaskOutputStore {
    inner: Arc<Mutex<TaskOutputStoreInner>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskOutputRead {
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    pub next_offset: usize,
    pub bytes_read: usize,
    pub bytes_total: usize,
    pub omitted_prefix_bytes: usize,
    pub stdout_prefix_bytes: usize,
    pub stderr_prefix_bytes: usize,
}

#[derive(Clone, Debug)]
struct TaskOutputBuffer {
    chunks: Vec<TaskOutputChunk>,
    bytes_total: usize,
    trimmed_stdout_bytes: usize,
    trimmed_stderr_bytes: usize,
}

#[derive(Debug)]
struct TaskOutputStoreInner {
    max_retained_bytes: usize,
    buffers: HashMap<String, TaskOutputBuffer>,
    archive: Option<Arc<Mutex<OutputArchive>>>,
    archive_error: Option<String>,
}

#[derive(Clone, Debug)]
struct TaskOutputChunk {
    stream: TaskOutputStream,
    start: usize,
    content: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskOutputStream {
    Stdout,
    Stderr,
}

impl TaskOutputStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_retained_bytes(max_retained_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskOutputStoreInner {
                max_retained_bytes,
                buffers: HashMap::new(),
                archive: None,
                archive_error: None,
            })),
        }
    }

    pub(crate) fn for_tasks(tasks: &crate::tasks::TaskRegistry) -> Self {
        let store = Self::new();
        let archive = tasks.output_storage_root().and_then(|root| {
            root.map(|root| OutputArchive::open(&root, tasks.session_id()))
                .transpose()
        });
        let mut inner = store.inner.lock().expect("task output store poisoned");
        match archive {
            Ok(archive) => inner.archive = archive,
            Err(error) => inner.archive_error = Some(error.to_string()),
        }
        drop(inner);
        store
    }

    pub(crate) fn register_shell(
        &self,
        shell_id: &str,
        task_id: &str,
        requested_pty: bool,
        effective_pty: bool,
    ) -> io::Result<()> {
        let mut inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        if let Some(archive) = &inner.archive {
            archive.lock().expect("output archive poisoned").register(
                shell_id,
                task_id,
                requested_pty,
                effective_pty,
            )?;
        }
        inner.buffers.entry(task_id.to_string()).or_default();
        Ok(())
    }

    pub(crate) fn archived_shell(&self, shell_id: &str) -> io::Result<ArchivedShell> {
        let inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        let archive = inner.archive.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no persistent output archive for this session",
            )
        })?;
        archive
            .lock()
            .expect("output archive poisoned")
            .shell(shell_id)
    }

    pub(crate) fn finish_shell(
        &self,
        task_id: &str,
        state: &str,
        exit_code: Option<i32>,
    ) -> io::Result<()> {
        let inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        if let Some(archive) = &inner.archive {
            archive
                .lock()
                .expect("output archive poisoned")
                .finish(task_id, state, exit_code)?;
        }
        Ok(())
    }

    pub(crate) fn set_cursor(&self, shell_id: &str, cursor: usize) -> io::Result<()> {
        let inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        if let Some(archive) = &inner.archive {
            archive
                .lock()
                .expect("output archive poisoned")
                .set_cursor(shell_id, cursor)?;
        }
        Ok(())
    }

    pub fn append_stdout(&self, task_id: &str, content: &str) -> io::Result<()> {
        self.append(task_id, TaskOutputStream::Stdout, content)
    }

    pub fn append_stderr(&self, task_id: &str, content: &str) -> io::Result<()> {
        self.append(task_id, TaskOutputStream::Stderr, content)
    }

    pub fn size(&self, task_id: &str) -> usize {
        self.inner
            .lock()
            .expect("task output store poisoned")
            .buffers
            .get(task_id)
            .map(|buffer| buffer.bytes_total)
            .unwrap_or(0)
    }

    pub fn remove(&self, task_id: &str) -> bool {
        // Process cleanup evicts the cache, not the session-owned archive.
        self.inner
            .lock()
            .expect("task output store poisoned")
            .buffers
            .remove(task_id)
            .is_some()
    }

    pub fn read_delta(
        &self,
        task_id: &str,
        from_offset: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        let inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        if let Some(archive) = &inner.archive {
            return archive.lock().expect("output archive poisoned").read(
                task_id,
                from_offset,
                max_bytes,
            );
        }
        inner.read_cached(task_id, from_offset, max_bytes)
    }

    pub(crate) fn read_cached(&self, task_id: &str) -> io::Result<TaskOutputRead> {
        self.read_cached_delta(task_id, 0, DEFAULT_TASK_OUTPUT_RETAINED_BYTES)
    }

    pub(crate) fn read_cached_delta(
        &self,
        task_id: &str,
        from: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        let inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        inner.read_cached(task_id, from, max_bytes.min(inner.max_retained_bytes))
    }

    pub fn tail(&self, task_id: &str, max_bytes: usize) -> io::Result<TaskOutputRead> {
        let inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        if let Some(archive) = &inner.archive {
            let archive = archive.lock().expect("output archive poisoned");
            return archive.tail(task_id, max_bytes);
        }
        let Some(buffer) = inner.buffers.get(task_id) else {
            return Ok(TaskOutputRead::empty(0));
        };
        let raw_start = buffer.bytes_total.saturating_sub(max_bytes);
        let raw_start = raw_start.max(buffer.retained_start());
        let start = buffer.next_readable_offset(raw_start);
        Ok(buffer.read_range(start, buffer.bytes_total, start))
    }

    fn append(&self, task_id: &str, stream: TaskOutputStream, content: &str) -> io::Result<()> {
        if content.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().expect("task output store poisoned");
        inner.check_archive()?;
        if let Some(archive) = inner.archive.clone()
            && let Err(error) = archive
                .lock()
                .expect("output archive poisoned")
                .append(task_id, stream, content)
        {
            inner.archive_error = Some(format!("task output persistence failed: {error}"));
            return Err(error);
        }
        let max_retained_bytes = inner.max_retained_bytes;
        let buffer = inner.buffers.entry(task_id.to_string()).or_default();
        let mut remaining = content;
        while !remaining.is_empty() {
            let end = utf8_ceil(remaining, remaining.len().min(TASK_OUTPUT_CHUNK_BYTES));
            buffer.append(stream, &remaining[..end]);
            buffer.trim_to_budget(max_retained_bytes);
            remaining = &remaining[end..];
        }
        if inner.archive.is_some() {
            let retained = inner
                .buffers
                .values()
                .map(|buffer| buffer.bytes_total - buffer.retained_start())
                .sum::<usize>();
            let mut excess = retained.saturating_sub(DEFAULT_TASK_OUTPUT_RETAINED_BYTES);
            for buffer in inner.buffers.values_mut() {
                if excess == 0 {
                    break;
                }
                let retained = buffer.bytes_total - buffer.retained_start();
                let target = retained.saturating_sub(excess);
                buffer.trim_to_budget(target);
                excess = excess
                    .saturating_sub(retained - (buffer.bytes_total - buffer.retained_start()));
            }
            let mut excess_chunks = inner
                .buffers
                .values()
                .map(|buffer| buffer.chunks.len())
                .sum::<usize>()
                .saturating_sub(MAX_RETAINED_CHUNKS);
            for buffer in inner.buffers.values_mut() {
                if excess_chunks == 0 {
                    break;
                }
                let remove = excess_chunks.min(buffer.chunks.len());
                let start = buffer
                    .chunks
                    .get(remove)
                    .map_or(buffer.bytes_total, |chunk| chunk.start);
                buffer.trim_to_budget(buffer.bytes_total - start);
                excess_chunks -= remove;
            }
        }
        Ok(())
    }
}

impl TaskOutputStoreInner {
    fn check_archive(&self) -> io::Result<()> {
        if let Some(error) = &self.archive_error {
            return Err(io::Error::other(error.clone()));
        }
        Ok(())
    }

    fn read_cached(
        &self,
        task_id: &str,
        from_offset: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        let Some(buffer) = self.buffers.get(task_id) else {
            return Ok(TaskOutputRead::empty(from_offset));
        };
        if from_offset > buffer.bytes_total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output_offset exceeds output_bytes_total",
            ));
        }
        let start = from_offset.max(buffer.retained_start());
        let end = start.saturating_add(max_bytes).min(buffer.bytes_total);
        Ok(buffer.read_range(start, end, start.saturating_sub(from_offset)))
    }
}

impl Default for TaskOutputStore {
    fn default() -> Self {
        Self::with_max_retained_bytes(DEFAULT_TASK_OUTPUT_RETAINED_BYTES)
    }
}

impl TaskOutputRead {
    fn empty(offset: usize) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            combined: String::new(),
            next_offset: offset,
            bytes_read: 0,
            bytes_total: offset,
            omitted_prefix_bytes: 0,
            stdout_prefix_bytes: 0,
            stderr_prefix_bytes: 0,
        }
    }
}

impl Default for TaskOutputBuffer {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            bytes_total: 0,
            trimmed_stdout_bytes: 0,
            trimmed_stderr_bytes: 0,
        }
    }
}

impl TaskOutputBuffer {
    fn append(&mut self, stream: TaskOutputStream, content: &str) {
        let start = self.bytes_total;
        self.bytes_total = self.bytes_total.saturating_add(content.len());
        self.chunks.push(TaskOutputChunk {
            stream,
            start,
            content: content.to_string(),
        });
    }

    fn retained_start(&self) -> usize {
        self.chunks
            .first()
            .map(|chunk| chunk.start)
            .unwrap_or(self.bytes_total)
    }

    fn read_range(&self, start: usize, end: usize, omitted_prefix_bytes: usize) -> TaskOutputRead {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut combined = String::new();
        let (stdout_prefix_bytes, stderr_prefix_bytes) = self.stream_prefix_bytes(start);
        let mut next_offset = start;
        for chunk in &self.chunks {
            let chunk_start = chunk.start;
            let chunk_end = chunk.start + chunk.content.len();
            if chunk_end <= start || chunk_start >= end {
                continue;
            }
            let local_start = start.saturating_sub(chunk_start);
            let local_end = end.min(chunk_end) - chunk_start;
            let local_start = utf8_ceil(&chunk.content, local_start);
            let local_end = utf8_ceil(&chunk.content, local_end);
            if local_start >= local_end {
                next_offset = next_offset.max(chunk_start + local_end);
                continue;
            }
            let text = &chunk.content[local_start..local_end];
            match chunk.stream {
                TaskOutputStream::Stdout => stdout.push_str(text),
                TaskOutputStream::Stderr => stderr.push_str(text),
            }
            combined.push_str(text);
            next_offset = chunk_start + local_end;
        }
        TaskOutputRead {
            stdout,
            stderr,
            combined,
            next_offset,
            bytes_read: next_offset.saturating_sub(start),
            bytes_total: self.bytes_total,
            omitted_prefix_bytes,
            stdout_prefix_bytes,
            stderr_prefix_bytes,
        }
    }

    fn stream_prefix_bytes(&self, offset: usize) -> (usize, usize) {
        let mut stdout = self.trimmed_stdout_bytes;
        let mut stderr = self.trimmed_stderr_bytes;
        for chunk in &self.chunks {
            let chunk_start = chunk.start;
            let chunk_end = chunk.start + chunk.content.len();
            if chunk_start >= offset {
                break;
            }
            let prefix_end = offset.min(chunk_end) - chunk_start;
            match chunk.stream {
                TaskOutputStream::Stdout => stdout = stdout.saturating_add(prefix_end),
                TaskOutputStream::Stderr => stderr = stderr.saturating_add(prefix_end),
            }
            if chunk_end >= offset {
                break;
            }
        }
        (stdout, stderr)
    }

    fn trim_to_budget(&mut self, max_retained_bytes: usize) {
        let mut retained_start = self.bytes_total.saturating_sub(max_retained_bytes);
        if self.chunks.len() > MAX_RETAINED_CHUNKS {
            retained_start =
                retained_start.max(self.chunks[self.chunks.len() - MAX_RETAINED_CHUNKS].start);
        }
        while let Some(chunk) = self.chunks.first_mut() {
            let chunk_end = chunk.start + chunk.content.len();
            if chunk_end <= retained_start {
                record_trimmed_stream_bytes(
                    &mut self.trimmed_stdout_bytes,
                    &mut self.trimmed_stderr_bytes,
                    chunk.content.len(),
                    chunk.stream,
                );
                self.chunks.remove(0);
                continue;
            }
            if retained_start <= chunk.start {
                break;
            }

            let local_start = utf8_ceil(&chunk.content, retained_start - chunk.start);
            if local_start >= chunk.content.len() {
                record_trimmed_stream_bytes(
                    &mut self.trimmed_stdout_bytes,
                    &mut self.trimmed_stderr_bytes,
                    chunk.content.len(),
                    chunk.stream,
                );
                self.chunks.remove(0);
                continue;
            }
            record_trimmed_stream_bytes(
                &mut self.trimmed_stdout_bytes,
                &mut self.trimmed_stderr_bytes,
                local_start,
                chunk.stream,
            );
            chunk.content = chunk.content[local_start..].to_string();
            chunk.start += local_start;
            break;
        }
        if self.chunks.is_empty()
            || self.chunks.capacity() > self.chunks.len().saturating_mul(2).max(64)
        {
            self.chunks.shrink_to_fit();
        }
    }

    fn next_readable_offset(&self, offset: usize) -> usize {
        for chunk in &self.chunks {
            let chunk_start = chunk.start;
            let chunk_end = chunk.start + chunk.content.len();
            if offset <= chunk_start {
                return chunk_start;
            }
            if offset < chunk_end {
                let local = utf8_ceil(&chunk.content, offset - chunk_start);
                let absolute = chunk_start + local;
                if chunk.content[local..].starts_with('\n') {
                    return absolute + 1;
                }
                return absolute;
            }
        }
        self.bytes_total
    }
}

fn record_trimmed_stream_bytes(
    trimmed_stdout_bytes: &mut usize,
    trimmed_stderr_bytes: &mut usize,
    len: usize,
    stream: TaskOutputStream,
) {
    match stream {
        TaskOutputStream::Stdout => {
            *trimmed_stdout_bytes = trimmed_stdout_bytes.saturating_add(len);
        }
        TaskOutputStream::Stderr => {
            *trimmed_stderr_bytes = trimmed_stderr_bytes.saturating_add(len);
        }
    }
}

fn utf8_ceil(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_delta_preserves_observed_chunk_order() {
        let store = TaskOutputStore::new();
        store.append_stdout("task-1", "first").unwrap();
        store.append_stderr("task-1", "-second").unwrap();
        store.append_stdout("task-1", "-third").unwrap();

        let output = store.read_delta("task-1", 0, usize::MAX).unwrap();

        assert_eq!(output.stdout, "first-third");
        assert_eq!(output.stderr, "-second");
        assert_eq!(output.combined, "first-second-third");
    }

    #[test]
    fn persistent_store_keeps_output_before_memory_eviction_and_after_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = crate::tasks::TaskRegistry::new_persistent(
            "output-large".to_string(),
            temp.path().join("tasks"),
        )
        .unwrap();
        let total = DEFAULT_TASK_OUTPUT_RETAINED_BYTES + 8192;
        {
            let store = TaskOutputStore::for_tasks(&tasks);
            store
                .register_shell("shell-large", "task-large", false, false)
                .unwrap();
            let chunk = "x".repeat(8192);
            for _ in 0..total / chunk.len() {
                store.append_stdout("task-large", &chunk).unwrap();
            }
            let cached = store.read_cached("task-large").unwrap();
            assert_eq!(cached.omitted_prefix_bytes, 8192);
            assert_eq!(
                store.read_delta("task-large", 0, 5).unwrap().combined,
                "xxxxx"
            );
            store.finish_shell("task-large", "exited", Some(0)).unwrap();
            store.remove("task-large");
            assert_eq!(store.size("task-large"), 0);
            assert_eq!(
                store
                    .read_delta("task-large", 0, 5)
                    .unwrap()
                    .omitted_prefix_bytes,
                0
            );
        }
        let reopened = TaskOutputStore::for_tasks(&tasks);
        assert_eq!(
            reopened.archived_shell("shell-large").unwrap().state,
            "exited"
        );
        let last = reopened.read_delta("task-large", total - 5, 5).unwrap();
        assert_eq!(last.combined, "xxxxx");
        assert_eq!(last.next_offset, total);
    }

    #[test]
    fn stores_are_session_scoped_and_live_storage_tampering_is_safe() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let one =
            crate::tasks::TaskRegistry::new_persistent("one".to_string(), root.clone()).unwrap();
        let two = crate::tasks::TaskRegistry::new_persistent("two".to_string(), root).unwrap();
        let store = TaskOutputStore::for_tasks(&one);
        store
            .register_shell("shell-one", "task-one", false, false)
            .unwrap();
        store.append_stdout("task-one", "secret").unwrap();
        let other = TaskOutputStore::for_tasks(&two);
        assert!(other.archived_shell("shell-one").is_err());
        assert!(other.read_delta("task-one", 0, 10).is_err());
        let database = one
            .output_storage_root()
            .unwrap()
            .unwrap()
            .join("archive.sqlite3");
        #[cfg(unix)]
        {
            std::fs::remove_file(database).unwrap();
            assert!(store.append_stdout("task-one", "lost").is_err());
            assert!(store.read_delta("task-one", 0, 10).is_err());
            assert!(store.read_cached("task-one").is_err());
        }
        #[cfg(windows)]
        {
            assert!(std::fs::remove_file(database).is_err());
            assert_eq!(
                store.read_delta("task-one", 0, 10).unwrap().combined,
                "secret"
            );
        }
    }

    #[test]
    fn store_reopening_in_process_does_not_interrupt_live_owner() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = crate::tasks::TaskRegistry::new_persistent(
            "live".to_string(),
            temp.path().join("tasks"),
        )
        .unwrap();
        let first = TaskOutputStore::for_tasks(&tasks);
        first
            .register_shell("shell-one", "task-one", false, false)
            .unwrap();
        let second = TaskOutputStore::for_tasks(&tasks);
        assert_eq!(second.archived_shell("shell-one").unwrap().state, "running");
        first.append_stdout("task-one", "shared").unwrap();
        assert_eq!(
            second.read_delta("task-one", 0, 10).unwrap().combined,
            "shared"
        );
    }

    #[test]
    fn output_storage_rejects_ambiguous_raw_session_ids() {
        for id in ["", ".", "..", "../other", "a/b", "a\\b"] {
            let tasks = crate::tasks::TaskRegistry::new(id.to_string());
            let store = TaskOutputStore::for_tasks(&tasks);
            assert!(
                store
                    .register_shell("shell-one", "task-one", false, false)
                    .is_err(),
                "{id}"
            );
        }
    }
}
