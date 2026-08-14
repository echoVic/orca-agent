#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::diff_highlight::{
    ParsedDiff, RefinedDiffStyles, compute_parsed_diff_file_scoped_styles,
};
use crate::syntax_highlight::{MAX_HIGHLIGHT_BYTES, SyntaxTheme, content_within_limits};
use crate::terminal_capabilities::TerminalColorLevel;

#[derive(Clone, Debug)]
pub(crate) struct EditHighlightJob {
    pub(crate) job_id: u64,
    pub(crate) tool_id: String,
    pub(crate) message_index: usize,
    pub(crate) message_revision: u64,
    pub(crate) syntax_theme_revision: u64,
    pub(crate) syntax_theme: SyntaxTheme,
    pub(crate) syntax_color_level: TerminalColorLevel,
    pub(crate) absolute_path: PathBuf,
    pub(crate) display_path: String,
    pub(crate) parsed: ParsedDiff,
}

#[derive(Clone, Debug)]
pub(crate) enum EditHighlightOutcome {
    Ready { styles: Arc<RefinedDiffStyles> },
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct EditHighlightResult {
    pub(crate) job: EditHighlightJob,
    pub(crate) outcome: EditHighlightOutcome,
}

pub(crate) struct DrainResults {
    pub(crate) results: Vec<EditHighlightResult>,
    pub(crate) disconnected: bool,
}

fn same_job_identity(left: &EditHighlightJob, right: &EditHighlightJob) -> bool {
    left.job_id == right.job_id
        && left.tool_id == right.tool_id
        && left.message_index == right.message_index
        && left.message_revision == right.message_revision
        && left.syntax_theme_revision == right.syntax_theme_revision
        && left.syntax_theme == right.syntax_theme
        && left.syntax_color_level == right.syntax_color_level
        && left.absolute_path == right.absolute_path
        && left.display_path == right.display_path
        && left.parsed == right.parsed
}

fn coalesce_jobs(
    first: EditHighlightJob,
    queued: impl IntoIterator<Item = EditHighlightJob>,
) -> Vec<EditHighlightJob> {
    coalesce_jobs_until_shutdown(first, queued, || false)
        .expect("shutdown callback never requests cancellation")
}

fn coalesce_jobs_until_shutdown(
    first: EditHighlightJob,
    queued: impl IntoIterator<Item = EditHighlightJob>,
    shutdown_requested: impl Fn() -> bool,
) -> Option<Vec<EditHighlightJob>> {
    if shutdown_requested() {
        return None;
    }
    let mut positions = HashMap::new();
    let mut jobs = Vec::new();

    positions.insert(first.tool_id.clone(), 0);
    jobs.push(first);

    let mut queued = queued.into_iter();
    loop {
        if shutdown_requested() {
            return None;
        }
        let Some(job) = queued.next() else {
            break;
        };
        if let Some(position) = positions.get(&job.tool_id).copied() {
            jobs[position] = job;
        } else {
            positions.insert(job.tool_id.clone(), jobs.len());
            jobs.push(job);
        }
    }

    Some(jobs)
}

fn read_capped_utf8_from(reader: impl Read, is_file: bool, metadata_len: u64) -> Option<String> {
    if !is_file || metadata_len > MAX_HIGHLIGHT_BYTES as u64 {
        return None;
    }

    let mut bytes = Vec::with_capacity(metadata_len as usize);
    reader
        .take(MAX_HIGHLIGHT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }

    let text = String::from_utf8(bytes).ok()?;
    content_within_limits(&text).then_some(text)
}

fn read_capped_utf8_with(
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<File>,
) -> Option<String> {
    let file = open(path).ok()?;
    let metadata = file.metadata().ok()?;
    read_capped_utf8_from(file, metadata.is_file(), metadata.len())
}

fn read_capped_utf8(path: &Path) -> Option<String> {
    let file = orca_platform::fs::open_nofollow_nonblocking(path).ok()?;
    let metadata = file.metadata().ok()?;
    read_capped_utf8_from(file, metadata.is_file(), metadata.len())
}

fn run_job(job: &EditHighlightJob) -> EditHighlightOutcome {
    let Some(file_text) = read_capped_utf8(&job.absolute_path) else {
        return EditHighlightOutcome::Failed;
    };
    compute_parsed_diff_file_scoped_styles(
        Path::new(&job.display_path),
        &file_text,
        &job.parsed,
        job.syntax_theme,
        job.syntax_color_level,
    )
    .map(|styles| EditHighlightOutcome::Ready {
        styles: Arc::new(styles),
    })
    .unwrap_or(EditHighlightOutcome::Failed)
}

fn worker_loop(
    job_rx: Receiver<EditHighlightJob>,
    result_tx: Sender<EditHighlightResult>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        let Ok(first) = job_rx.recv() else {
            return;
        };
        let Some(jobs) = coalesce_jobs_until_shutdown(first, job_rx.try_iter(), || {
            shutdown.load(Ordering::Acquire)
        }) else {
            return;
        };
        for job in jobs {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            let outcome = run_job(&job);
            if result_tx
                .send(EditHighlightResult { job, outcome })
                .is_err()
            {
                return;
            }
        }
    }
}

fn spawn_worker() -> io::Result<(
    Sender<EditHighlightJob>,
    Receiver<EditHighlightResult>,
    JoinHandle<()>,
    Arc<AtomicBool>,
)> {
    spawn_worker_with_runner(worker_loop)
}

fn spawn_worker_with_runner(
    runner: impl FnOnce(Receiver<EditHighlightJob>, Sender<EditHighlightResult>, Arc<AtomicBool>)
    + Send
    + 'static,
) -> io::Result<(
    Sender<EditHighlightJob>,
    Receiver<EditHighlightResult>,
    JoinHandle<()>,
    Arc<AtomicBool>,
)> {
    spawn_worker_with(runner, |worker| {
        thread::Builder::new()
            .name("orca-edit-highlight".to_owned())
            .spawn(worker)
    })
}

fn spawn_worker_with(
    runner: impl FnOnce(Receiver<EditHighlightJob>, Sender<EditHighlightResult>, Arc<AtomicBool>)
    + Send
    + 'static,
    spawner: impl FnOnce(Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>>,
) -> io::Result<(
    Sender<EditHighlightJob>,
    Receiver<EditHighlightResult>,
    JoinHandle<()>,
    Arc<AtomicBool>,
)> {
    let (job_tx, job_rx) = crossbeam_channel::unbounded();
    let (result_tx, result_rx) = crossbeam_channel::unbounded();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = spawner(Box::new(move || runner(job_rx, result_tx, worker_shutdown)))?;
    Ok((job_tx, result_rx, worker, shutdown))
}

pub(crate) struct EditHighlightRuntime {
    job_tx: Option<Sender<EditHighlightJob>>,
    result_rx: Option<Receiver<EditHighlightResult>>,
    worker: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    pending: HashMap<String, EditHighlightJob>,
    next_job_id: u64,
    #[cfg(test)]
    successful_submit_count: usize,
}

impl EditHighlightRuntime {
    pub(crate) fn new() -> io::Result<Self> {
        Self::new_with_channels(spawn_worker()?)
    }

    fn new_with_channels(
        (job_tx, result_rx, worker, shutdown): (
            Sender<EditHighlightJob>,
            Receiver<EditHighlightResult>,
            JoinHandle<()>,
            Arc<AtomicBool>,
        ),
    ) -> io::Result<Self> {
        Ok(Self {
            job_tx: Some(job_tx),
            result_rx: Some(result_rx),
            worker: Some(worker),
            shutdown,
            pending: HashMap::new(),
            next_job_id: 1,
            #[cfg(test)]
            successful_submit_count: 0,
        })
    }

    #[cfg(test)]
    fn new_with_worker(
        runner: impl FnOnce(Receiver<EditHighlightJob>, Sender<EditHighlightResult>) + Send + 'static,
    ) -> io::Result<Self> {
        Self::new_with_channels(spawn_worker_with_runner(
            move |job_rx, result_tx, _shutdown| {
                runner(job_rx, result_tx);
            },
        )?)
    }

    #[cfg(test)]
    fn new_with_shutdown_worker(
        runner: impl FnOnce(Receiver<EditHighlightJob>, Sender<EditHighlightResult>, Arc<AtomicBool>)
        + Send
        + 'static,
    ) -> io::Result<Self> {
        Self::new_with_channels(spawn_worker_with_runner(runner)?)
    }

    #[cfg(test)]
    fn new_with_spawner(
        spawner: impl FnOnce(Box<dyn FnOnce() + Send + 'static>) -> io::Result<JoinHandle<()>>,
    ) -> io::Result<Self> {
        Self::new_with_channels(spawn_worker_with(worker_loop, spawner)?)
    }

    pub(crate) fn allocate_job_id(&mut self) -> u64 {
        let job_id = self.next_job_id.max(1);
        self.next_job_id = job_id.wrapping_add(1).max(1);
        job_id
    }

    pub(crate) fn submit(&mut self, job: EditHighlightJob) -> bool {
        let submitted = self
            .job_tx
            .as_ref()
            .is_some_and(|job_tx| job_tx.send(job.clone()).is_ok());
        if !submitted {
            self.job_tx = None;
            self.pending.clear();
            return false;
        }
        #[cfg(test)]
        {
            self.successful_submit_count = self.successful_submit_count.saturating_add(1);
        }
        self.pending.insert(job.tool_id.clone(), job);
        true
    }

    pub(crate) fn drain_results(&mut self) -> DrainResults {
        let mut results = Vec::new();
        let result_rx = self
            .result_rx
            .as_ref()
            .expect("edit highlight result receiver available while runtime is live");
        let disconnected = loop {
            match result_rx.try_recv() {
                Ok(result) => results.push(result),
                Err(TryRecvError::Empty) => break false,
                Err(TryRecvError::Disconnected) => break true,
            }
        };
        if disconnected {
            self.pending.retain(|_, pending| {
                results
                    .iter()
                    .any(|result| same_job_identity(pending, &result.job))
            });
        }
        DrainResults {
            results,
            disconnected,
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn pending_matches(&self, job: &EditHighlightJob) -> bool {
        self.pending
            .get(&job.tool_id)
            .is_some_and(|pending| same_job_identity(pending, job))
    }

    pub(crate) fn finish_pending(&mut self, job: &EditHighlightJob) -> bool {
        if !self.pending_matches(job) {
            return false;
        }
        self.pending.remove(&job.tool_id);
        true
    }

    pub(crate) fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn cancel_pending_for_message(
        &mut self,
        message_index: usize,
        message_revision: u64,
    ) -> bool {
        let before = self.pending.len();
        self.pending.retain(|_, pending| {
            pending.message_index != message_index || pending.message_revision != message_revision
        });
        self.pending.len() != before
    }

    #[cfg(test)]
    pub(crate) fn pending_job(&self, tool_id: &str) -> Option<EditHighlightJob> {
        self.pending.get(tool_id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn successful_submit_count(&self) -> usize {
        self.successful_submit_count
    }

    #[cfg(test)]
    pub(crate) fn disconnected_for_test() -> Self {
        let (job_tx, job_rx) = crossbeam_channel::unbounded();
        drop(job_rx);
        let (_result_tx, result_rx) = crossbeam_channel::unbounded();
        Self {
            job_tx: Some(job_tx),
            result_rx: Some(result_rx),
            worker: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            pending: HashMap::new(),
            next_job_id: 1,
            successful_submit_count: 0,
        }
    }
}

impl Drop for EditHighlightRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.job_tx.take();
        self.result_rx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use crossbeam_channel::RecvTimeoutError;
    use ratatui::text::Span;

    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::process::Command;

    use super::{
        DrainResults, EditHighlightJob, EditHighlightOutcome, EditHighlightResult,
        EditHighlightRuntime, coalesce_jobs, coalesce_jobs_until_shutdown, read_capped_utf8_from,
        read_capped_utf8_with, run_job, spawn_worker,
    };
    use crate::diff_highlight::{RefinedDiffStyles, parse_unified_diff};
    use crate::syntax_highlight::{
        MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINE_BYTES, MAX_HIGHLIGHT_LINES, SyntaxTheme,
        highlighter_for_path,
    };
    use crate::terminal_capabilities::{TerminalColorLevel, syntax_style_revision};

    const MATCHING_DIFF: &str = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value: int = 1
+value: int = 42
";

    fn job(
        job_id: u64,
        tool_id: &str,
        absolute_path: PathBuf,
        display_path: &str,
        diff: &str,
    ) -> EditHighlightJob {
        EditHighlightJob {
            job_id,
            tool_id: tool_id.to_owned(),
            message_index: 2,
            message_revision: 7,
            syntax_theme_revision: syntax_style_revision(
                SyntaxTheme::OneHalfDark,
                TerminalColorLevel::TrueColor,
            ),
            syntax_theme: SyntaxTheme::OneHalfDark,
            syntax_color_level: TerminalColorLevel::TrueColor,
            absolute_path,
            display_path: display_path.to_owned(),
            parsed: parse_unified_diff(diff),
        }
    }

    fn assert_same_job(actual: &EditHighlightJob, expected: &EditHighlightJob) {
        assert_eq!(actual.job_id, expected.job_id);
        assert_eq!(actual.tool_id, expected.tool_id);
        assert_eq!(actual.message_index, expected.message_index);
        assert_eq!(actual.message_revision, expected.message_revision);
        assert_eq!(actual.syntax_theme_revision, expected.syntax_theme_revision);
        assert_eq!(actual.syntax_theme, expected.syntax_theme);
        assert_eq!(actual.syntax_color_level, expected.syntax_color_level);
        assert_eq!(actual.absolute_path, expected.absolute_path);
        assert_eq!(actual.display_path, expected.display_path);
        assert_eq!(actual.parsed, expected.parsed);
    }

    fn ready_styles(outcome: EditHighlightOutcome) -> Arc<RefinedDiffStyles> {
        match outcome {
            EditHighlightOutcome::Ready { styles } => styles,
            EditHighlightOutcome::Failed => panic!("expected ready highlight result"),
        }
    }

    fn assert_failed(job: &EditHighlightJob) {
        assert!(matches!(run_job(job), EditHighlightOutcome::Failed));
    }

    fn exact_byte_limit_text() -> String {
        let mut text = String::from("value: int = 42\n");
        let full_line = format!("{}\n", "x".repeat(MAX_HIGHLIGHT_LINE_BYTES - 1));
        while text.len() + full_line.len() <= MAX_HIGHLIGHT_BYTES {
            text.push_str(&full_line);
        }
        text.push_str(&"x".repeat(MAX_HIGHLIGHT_BYTES - text.len()));
        assert_eq!(text.len(), MAX_HIGHLIGHT_BYTES);
        assert!(text.lines().count() < MAX_HIGHLIGHT_LINES);
        assert!(
            text.lines()
                .all(|line| line.len() <= MAX_HIGHLIGHT_LINE_BYTES)
        );
        text
    }

    struct CountingReader {
        remaining: usize,
        bytes_read: Arc<AtomicUsize>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = buffer.len().min(self.remaining);
            buffer[..count].fill(b'x');
            self.remaining -= count;
            self.bytes_read.fetch_add(count, Ordering::SeqCst);
            Ok(count)
        }
    }

    struct PanicReader;

    impl Read for PanicReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            panic!("metadata guard must reject before reading the handle")
        }
    }

    #[test]
    fn coalescing_keeps_latest_full_job_in_original_fifo_key_order() {
        let first = job(1, "edit-a", PathBuf::from("/first-a"), "first-a.py", "");
        let mut latest_a = job(
            2,
            "edit-a",
            PathBuf::from("/latest-a"),
            "latest-a.rs",
            "--- a/latest-a.rs\n+++ b/latest-a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        );
        latest_a.message_index = 12;
        latest_a.message_revision = 17;
        latest_a.syntax_theme = SyntaxTheme::OneHalfLight;
        latest_a.syntax_color_level = TerminalColorLevel::Ansi256;
        latest_a.syntax_theme_revision =
            syntax_style_revision(SyntaxTheme::OneHalfLight, latest_a.syntax_color_level);
        let latest_b = job(
            3,
            "edit-b",
            PathBuf::from("/latest-b"),
            "latest-b.py",
            MATCHING_DIFF,
        );

        let coalesced = coalesce_jobs(first, [latest_a.clone(), latest_b.clone()]);

        assert_eq!(coalesced.len(), 2);
        assert_same_job(&coalesced[0], &latest_a);
        assert_same_job(&coalesced[1], &latest_b);
    }

    #[test]
    fn coalescing_replacement_after_another_key_keeps_original_key_position() {
        let first = job(1, "edit-a", PathBuf::from("/a1"), "a1.py", "");
        let middle_b = job(3, "edit-b", PathBuf::from("/b"), "b.py", "");
        let latest_a = job(4, "edit-a", PathBuf::from("/a4"), "a4.py", MATCHING_DIFF);

        let coalesced = coalesce_jobs(
            first,
            [
                job(2, "edit-a", PathBuf::from("/a2"), "a2.py", ""),
                middle_b.clone(),
                latest_a.clone(),
            ],
        );

        assert_eq!(coalesced.len(), 2);
        assert_same_job(&coalesced[0], &latest_a);
        assert_same_job(&coalesced[1], &middle_b);
    }

    #[test]
    fn run_job_returns_exact_ready_style_map_for_matching_python_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "value: int = 42\n").unwrap();
        let job = job(1, "edit-a", path, "src/item.py", MATCHING_DIFF);

        let styles = ready_styles(run_job(&job));
        let mut highlighter = highlighter_for_path(
            Path::new("src/item.py"),
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("known Python syntax");
        let expected = highlighter
            .highlight_line("value: int = 42")
            .expect("highlighted Python line");

        assert_eq!(styles.len(), 1);
        assert_eq!(styles.get(&1), Some(&expected));
        assert_eq!(
            styles[&1]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "value: int = 42"
        );
    }

    #[test]
    fn run_job_fails_for_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        assert_failed(&job(
            1,
            "missing",
            directory.path().join("missing.py"),
            "src/item.py",
            MATCHING_DIFF,
        ));
    }

    #[test]
    fn run_job_fails_for_non_file_path() {
        let directory = tempfile::tempdir().unwrap();
        assert_failed(&job(
            1,
            "directory",
            directory.path().to_path_buf(),
            "src/item.py",
            MATCHING_DIFF,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn run_job_rejects_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.py");
        let link = directory.path().join("item.py");
        std::fs::write(&target, "value: int = 42\n").unwrap();
        symlink(&target, &link).unwrap();

        assert_failed(&job(1, "symlink", link, "src/item.py", MATCHING_DIFF));
    }

    #[test]
    fn run_job_fails_for_non_utf8_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("binary.py");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();

        assert_failed(&job(1, "binary", path, "src/item.py", MATCHING_DIFF));
    }

    #[test]
    fn metadata_length_above_cap_fails_before_file_read() {
        let result = read_capped_utf8_from(PanicReader, true, MAX_HIGHLIGHT_BYTES as u64 + 1);

        assert!(result.is_none());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("too-large.py");
        let mut text = exact_byte_limit_text();
        text.push('x');
        std::fs::write(&path, text).unwrap();
        assert_failed(&job(1, "too-large", path, "src/item.py", MATCHING_DIFF));
    }

    #[test]
    fn bounded_reader_rejects_growth_after_cap_plus_one_bytes() {
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let reader = CountingReader {
            remaining: MAX_HIGHLIGHT_BYTES + 128,
            bytes_read: Arc::clone(&bytes_read),
        };

        let result = read_capped_utf8_from(reader, true, MAX_HIGHLIGHT_BYTES as u64);

        assert!(result.is_none());
        assert_eq!(bytes_read.load(Ordering::SeqCst), MAX_HIGHLIGHT_BYTES + 1);
    }

    #[cfg(unix)]
    #[test]
    fn opened_regular_handle_survives_path_replacement_with_fifo() {
        use std::os::unix::fs::FileTypeExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "value: int = 42\n").unwrap();

        let text = read_capped_utf8_with(&path, |path| {
            let file = std::fs::File::open(path)?;
            std::fs::remove_file(path)?;
            let status = Command::new("mkfifo").arg(path).status()?;
            if !status.success() {
                return Err(io::Error::other("mkfifo failed"));
            }
            Ok(file)
        });

        assert_eq!(text.as_deref(), Some("value: int = 42\n"));
        assert!(std::fs::metadata(&path).unwrap().file_type().is_fifo());
    }

    #[test]
    fn run_job_fails_for_more_than_max_actual_lines_without_trailing_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("too-many-lines.py");
        let mut text = String::from("value: int = 42\n");
        text.push_str(&"x\n".repeat(MAX_HIGHLIGHT_LINES - 1));
        text.push('x');
        assert_eq!(text.lines().count(), MAX_HIGHLIGHT_LINES + 1);
        assert!(text.len() < MAX_HIGHLIGHT_BYTES);
        std::fs::write(&path, text).unwrap();

        assert_failed(&job(
            1,
            "too-many-lines",
            path,
            "src/item.py",
            MATCHING_DIFF,
        ));
    }

    #[test]
    fn run_job_fails_for_matching_line_above_line_byte_cap() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("long-line.py");
        let source = "x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1);
        std::fs::write(&path, &source).unwrap();
        let diff = format!("--- /dev/null\n+++ b/long-line.py\n@@ -0,0 +1 @@\n+{source}\n");

        assert_failed(&job(1, "long-line", path, "long-line.py", &diff));
    }

    #[test]
    fn run_job_fails_when_post_edit_text_drifted_from_diff() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "value: int = 43\n").unwrap();

        assert_failed(&job(1, "drifted", path, "src/item.py", MATCHING_DIFF));
    }

    #[test]
    fn run_job_fails_for_unknown_syntax_with_matching_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.unknown");
        std::fs::write(&path, "value: int = 42\n").unwrap();

        assert_failed(&job(1, "unknown", path, "src/item.unknown", MATCHING_DIFF));
    }

    #[test]
    fn run_job_fails_for_multi_file_parsed_diff_with_matching_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "value: int = 42\n").unwrap();
        let diff = "\
--- a/first.py
+++ b/first.py
@@ -1 +1 @@
-value: int = 1
+value: int = 42
--- a/second.py
+++ b/second.py
@@ -1 +1 @@
-value: int = 1
+value: int = 42
";

        let job = job(1, "multi-file", path, "src/item.py", diff);
        assert!(job.parsed.has_multiple_files);
        assert_failed(&job);
    }

    #[test]
    fn run_job_accepts_exact_byte_line_count_and_line_length_boundaries() {
        let directory = tempfile::tempdir().unwrap();

        let exact_bytes_path = directory.path().join("exact-bytes.py");
        std::fs::write(&exact_bytes_path, exact_byte_limit_text()).unwrap();
        let exact_bytes = job(
            1,
            "exact-bytes",
            exact_bytes_path,
            "src/item.py",
            MATCHING_DIFF,
        );

        let exact_lines_path = directory.path().join("exact-lines.py");
        let mut exact_lines_text = String::from("value: int = 42\n");
        exact_lines_text.push_str(&"x\n".repeat(MAX_HIGHLIGHT_LINES - 1));
        assert_eq!(exact_lines_text.lines().count(), MAX_HIGHLIGHT_LINES);
        std::fs::write(&exact_lines_path, exact_lines_text).unwrap();
        let exact_lines = job(
            2,
            "exact-lines",
            exact_lines_path,
            "src/item.py",
            MATCHING_DIFF,
        );

        let exact_line_path = directory.path().join("exact-line.py");
        let exact_line_source = "x".repeat(MAX_HIGHLIGHT_LINE_BYTES);
        std::fs::write(&exact_line_path, &exact_line_source).unwrap();
        let exact_line_diff =
            format!("--- /dev/null\n+++ b/exact-line.py\n@@ -0,0 +1 @@\n+{exact_line_source}\n");
        let exact_line = job(
            3,
            "exact-line",
            exact_line_path,
            "exact-line.py",
            &exact_line_diff,
        );

        for boundary_job in [exact_bytes, exact_lines, exact_line] {
            assert!(
                matches!(run_job(&boundary_job), EditHighlightOutcome::Ready { .. }),
                "exact boundary rejected for {}",
                boundary_job.tool_id
            );
        }
    }

    #[test]
    fn delete_only_diff_returns_ready_with_empty_styles_for_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "").unwrap();
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -1 +0,0 @@
-value = 1
";

        let styles = ready_styles(run_job(&job(1, "delete-only", path, "item.py", diff)));

        assert!(styles.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn worker_rejects_fifo_without_blocking_the_only_worker() {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("item.py");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success());
        let submitted = job(40, "fifo", fifo.clone(), "src/item.py", MATCHING_DIFF);
        let (job_tx, result_rx, worker, _shutdown) = spawn_worker().expect("test worker");

        job_tx.send(submitted).unwrap();
        let result = match result_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                let mut fifo_guard = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                    .expect("open nonblocking FIFO cleanup guard");
                fifo_guard.write_all(b"x").unwrap();
                drop(fifo_guard);
                let _ = result_rx.recv_timeout(Duration::from_secs(1));
                drop(job_tx);
                worker.join().expect("blocked worker cleanup");
                panic!("FIFO blocked the edit highlight worker");
            }
            Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected without a result"),
        };

        assert!(matches!(result.outcome, EditHighlightOutcome::Failed));
        drop(job_tx);
        worker.join().expect("worker shutdown");
    }

    #[test]
    fn named_worker_thread_returns_one_owned_result_with_same_job_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "value: int = 42\n").unwrap();
        let expected = job(41, "edit-a", path, "src/item.py", MATCHING_DIFF);
        let (job_tx, result_rx, worker, _shutdown) = spawn_worker().expect("test worker");

        assert_eq!(worker.thread().name(), Some("orca-edit-highlight"));
        job_tx.send(expected.clone()).unwrap();
        let result = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker result");

        assert_same_job(&result.job, &expected);
        assert!(matches!(result.outcome, EditHighlightOutcome::Ready { .. }));
        drop(job_tx);
        worker.join().expect("worker shutdown");
    }

    #[test]
    fn worker_exits_when_job_channel_disconnects() {
        let (job_tx, result_rx, worker, _shutdown) = spawn_worker().expect("test worker");

        drop(job_tx);
        worker.join().expect("worker shutdown");

        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn drop_closes_job_channel_and_joins_worker() {
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let runtime = EditHighlightRuntime::new_with_worker(move |job_rx, _result_tx| {
            while job_rx.recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(50));
            worker_exited.store(true, Ordering::Release);
        })
        .unwrap();

        drop(runtime);

        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn drop_closes_result_channel_before_joining_worker() {
        let result_disconnected = Arc::new(AtomicBool::new(false));
        let worker_observation = Arc::clone(&result_disconnected);
        let mut runtime = EditHighlightRuntime::new_with_worker(move |job_rx, result_tx| {
            let submitted = job_rx.recv().expect("submitted job");
            std::thread::sleep(Duration::from_millis(50));
            let send_failed = result_tx
                .send(EditHighlightResult {
                    job: submitted,
                    outcome: EditHighlightOutcome::Failed,
                })
                .is_err();
            worker_observation.store(send_failed, Ordering::Release);
        })
        .unwrap();
        assert!(runtime.submit(job(1, "edit-1", PathBuf::from("/item.py"), "item.py", "")));

        drop(runtime);

        assert!(result_disconnected.load(Ordering::Acquire));
    }

    #[test]
    fn drop_fences_queued_backlog_before_joining_worker() {
        let (accepted_tx, accepted_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let queued_consumed = Arc::new(AtomicUsize::new(usize::MAX));
        let worker_consumed = Arc::clone(&queued_consumed);
        let mut runtime =
            EditHighlightRuntime::new_with_shutdown_worker(move |job_rx, _result_tx, shutdown| {
                let first = job_rx.recv().expect("first submitted job");
                accepted_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let queued_before = job_rx.len();
                let coalesced = coalesce_jobs_until_shutdown(first, job_rx.try_iter(), || {
                    shutdown.load(Ordering::Acquire)
                });
                worker_consumed.store(queued_before - job_rx.len(), Ordering::Release);
                assert!(coalesced.is_none());
            })
            .unwrap();
        assert!(runtime.submit(job(1, "edit-1", PathBuf::from("/item.py"), "item.py", "")));
        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker accepted first job");
        for job_id in 2..=1_002 {
            assert!(runtime.submit(job(
                job_id,
                &format!("edit-{job_id}"),
                PathBuf::from(format!("/{job_id}.py")),
                &format!("{job_id}.py"),
                ""
            )));
        }

        let shutdown = Arc::clone(&runtime.shutdown);
        let dropper = std::thread::spawn(move || drop(runtime));
        while !shutdown.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        dropper.join().unwrap();

        assert_eq!(queued_consumed.load(Ordering::Acquire), 0);
    }

    #[test]
    fn worker_exits_when_result_receiver_disconnects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("item.py");
        std::fs::write(&path, "value: int = 42\n").unwrap();
        let submitted = job(51, "edit-a", path, "src/item.py", MATCHING_DIFF);
        let (job_tx, result_rx, worker, _shutdown) = spawn_worker().expect("test worker");

        drop(result_rx);
        job_tx.send(submitted).unwrap();
        worker.join().expect("worker shutdown");
    }

    #[test]
    fn runtime_pending_replacement_and_finish_use_full_job_identity() {
        let mut runtime = EditHighlightRuntime::new().expect("test runtime");
        let first_a = job(1, "edit-a", PathBuf::from("/a1"), "a1.py", "");
        let latest_a = job(2, "edit-a", PathBuf::from("/a2"), "a2.py", MATCHING_DIFF);
        let b = job(3, "edit-b", PathBuf::from("/b"), "b.py", "");

        assert!(runtime.submit(first_a));
        assert!(runtime.submit(b.clone()));
        assert!(runtime.submit(latest_a.clone()));

        assert!(runtime.has_pending());
        assert_eq!(runtime.pending_count(), 2);
        assert_same_job(
            &runtime.pending_job("edit-a").expect("latest pending A"),
            &latest_a,
        );
        assert!(runtime.pending_matches(&latest_a));

        let mut stale_jobs = Vec::new();
        let mut stale = latest_a.clone();
        stale.tool_id = "edit-c".to_owned();
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.job_id += 1;
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.message_index += 1;
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.message_revision += 1;
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.syntax_theme_revision =
            syntax_style_revision(SyntaxTheme::OneHalfLight, stale.syntax_color_level);
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.syntax_theme = SyntaxTheme::OneHalfLight;
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.syntax_color_level = TerminalColorLevel::Ansi16;
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.absolute_path = PathBuf::from("/other");
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.display_path = "other.py".to_owned();
        stale_jobs.push(stale);
        let mut stale = latest_a.clone();
        stale.parsed =
            parse_unified_diff("--- a/a2.py\n+++ b/a2.py\n@@ -1 +1 @@\n-old\n+different\n");
        stale_jobs.push(stale);

        for stale in stale_jobs {
            assert!(!runtime.pending_matches(&stale));
            assert!(!runtime.finish_pending(&stale));
        }

        assert!(runtime.finish_pending(&latest_a));
        assert!(!runtime.pending_matches(&latest_a));
        assert_eq!(runtime.pending_count(), 1);
        assert!(runtime.finish_pending(&b));
        assert!(!runtime.has_pending());
        runtime.clear_pending();
        assert_eq!(runtime.pending_count(), 0);
    }

    #[test]
    fn cancel_pending_for_message_removes_only_exact_message_revision() {
        let mut runtime = EditHighlightRuntime::new().expect("test runtime");
        let a = job(1, "edit-a", PathBuf::from("/a"), "a.py", "");
        let mut b = job(2, "edit-b", PathBuf::from("/b"), "b.py", "");
        b.message_index += 1;
        b.message_revision += 1;
        assert!(runtime.submit(a.clone()));
        assert!(runtime.submit(b.clone()));

        assert!(!runtime.cancel_pending_for_message(a.message_index, a.message_revision + 1));
        assert!(runtime.pending_matches(&a));
        assert!(runtime.pending_matches(&b));

        assert!(runtime.cancel_pending_for_message(a.message_index, a.message_revision));
        assert!(!runtime.pending_matches(&a));
        assert!(runtime.pending_matches(&b));
        assert_eq!(runtime.pending_count(), 1);
    }

    #[test]
    fn allocate_job_id_wraps_from_max_to_one_without_returning_zero() {
        let mut runtime = EditHighlightRuntime::new().expect("test runtime");
        runtime.next_job_id = u64::MAX;

        assert_eq!(runtime.allocate_job_id(), u64::MAX);
        assert_eq!(runtime.allocate_job_id(), 1);
        assert_eq!(runtime.allocate_job_id(), 2);
    }

    #[test]
    fn failed_submit_clears_pending_and_does_not_insert_failed_job() {
        let (job_tx, job_rx) = crossbeam_channel::unbounded();
        drop(job_rx);
        let (_result_tx, result_rx) = crossbeam_channel::unbounded();
        let existing = job(1, "existing", PathBuf::from("/old"), "old.py", "");
        let mut runtime = EditHighlightRuntime {
            job_tx: Some(job_tx),
            result_rx: Some(result_rx),
            worker: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            pending: HashMap::from([(existing.tool_id.clone(), existing)]),
            next_job_id: 2,
            successful_submit_count: 0,
        };

        assert!(!runtime.submit(job(2, "failed", PathBuf::from("/new"), "new.py", "")));
        assert!(!runtime.has_pending());
        assert_eq!(runtime.pending_count(), 0);
        assert!(runtime.pending_job("failed").is_none());
    }

    #[test]
    fn successful_submit_count_tracks_sends_not_pending_replacements() {
        let mut runtime = EditHighlightRuntime::new().expect("test runtime");

        assert!(runtime.submit(job(1, "same-tool", PathBuf::from("/first"), "first.py", "")));
        assert!(runtime.submit(job(
            2,
            "same-tool",
            PathBuf::from("/second"),
            "second.py",
            ""
        )));

        assert_eq!(runtime.pending_count(), 1);
        assert_eq!(runtime.successful_submit_count(), 2);
    }

    #[test]
    fn disconnected_runtime_for_test_rejects_submit_without_counting_success() {
        let mut runtime = EditHighlightRuntime::disconnected_for_test();

        assert!(!runtime.submit(job(
            1,
            "disconnected",
            PathBuf::from("/item"),
            "item.py",
            ""
        )));
        assert_eq!(runtime.pending_count(), 0);
        assert_eq!(runtime.successful_submit_count(), 0);
    }

    #[test]
    fn spawn_failure_returns_error_without_constructing_a_runtime() {
        let error = match EditHighlightRuntime::new_with_spawner(|_| {
            Err(io::Error::other("injected spawn failure"))
        }) {
            Ok(_) => panic!("spawn failure must not construct a runtime"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "injected spawn failure");
    }

    #[test]
    fn accepted_job_then_worker_exit_reports_disconnect_and_clears_pending() {
        let (accepted_tx, accepted_rx) = crossbeam_channel::bounded(1);
        let (exited_tx, exited_rx) = crossbeam_channel::bounded(1);
        let mut runtime = EditHighlightRuntime::new_with_worker(move |job_rx, result_tx| {
            let accepted = job_rx.recv().expect("accepted job");
            accepted_tx.send(accepted).unwrap();
            drop(result_tx);
            exited_tx.send(()).unwrap();
        })
        .expect("test runtime");
        let submitted = job(1, "stranded", PathBuf::from("/old"), "old.py", "");

        assert!(runtime.submit(submitted.clone()));
        let accepted = accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker accepted job");
        assert_same_job(&accepted, &submitted);
        exited_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker exited");

        let DrainResults {
            results,
            disconnected,
        } = runtime.drain_results();

        assert!(results.is_empty());
        assert!(disconnected);
        assert!(!runtime.has_pending());
        assert_eq!(runtime.pending_count(), 0);
    }

    #[test]
    fn drain_results_returns_owned_nonblocking_vec() {
        let (job_tx, _job_rx) = crossbeam_channel::unbounded();
        let (result_tx, result_rx) = crossbeam_channel::unbounded();
        let mut runtime = EditHighlightRuntime {
            job_tx: Some(job_tx),
            result_rx: Some(result_rx),
            worker: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            pending: HashMap::new(),
            next_job_id: 1,
            successful_submit_count: 0,
        };

        let empty = runtime.drain_results();
        assert!(empty.results.is_empty());
        assert!(!empty.disconnected);

        for job_id in [1, 2] {
            let result_job = job(
                job_id,
                &format!("edit-{job_id}"),
                PathBuf::from(format!("/{job_id}")),
                "item.py",
                "",
            );
            result_tx
                .send(EditHighlightResult {
                    job: result_job,
                    outcome: EditHighlightOutcome::Failed,
                })
                .unwrap();
        }

        let drained = runtime.drain_results();
        runtime.clear_pending();

        assert!(!drained.disconnected);
        assert_eq!(
            drained
                .results
                .iter()
                .map(|result| result.job.job_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(runtime.drain_results().results.is_empty());
    }

    #[test]
    fn ready_result_contract_owns_arc_style_map() {
        let styles = Arc::new(RefinedDiffStyles::from([(
            1,
            vec![Span::raw("value: int = 42".to_owned())],
        )]));
        let result = EditHighlightResult {
            job: job(
                1,
                "edit-a",
                PathBuf::from("/item.py"),
                "item.py",
                MATCHING_DIFF,
            ),
            outcome: EditHighlightOutcome::Ready {
                styles: Arc::clone(&styles),
            },
        };

        let EditHighlightOutcome::Ready {
            styles: result_styles,
        } = result.outcome
        else {
            panic!("ready result");
        };
        assert!(Arc::ptr_eq(&styles, &result_styles));
    }
}
