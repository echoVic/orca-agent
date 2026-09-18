use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use orca_platform::process::ProcessJob;
use serde_json::Value;
use tempfile::TempDir;

const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const DROP_GRACE_TIMEOUT: Duration = Duration::from_millis(250);
const SIGNAL_GRACE_TIMEOUT: Duration = Duration::from_millis(250);
const READER_EOF_TIMEOUT: Duration = Duration::from_millis(250);
const READER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const READER_IDLE_POLL: Duration = Duration::from_millis(5);
const MAX_QUEUED_LINES: usize = 4_096;
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_EVENTS: usize = 4_096;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PROTOCOL_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSCRIPT_LINES: usize = 64;
const MAX_TRANSCRIPT_LINE_CHARS: usize = 2_048;
const MAX_DIAGNOSTIC_STDERR_BYTES: usize = 8 * 1024;

#[cfg(unix)]
const TERMINATE_SIGNAL: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const TERMINATE_SIGNAL: i32 = 0;
#[cfg(unix)]
const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 0;

#[derive(Debug)]
pub struct ServerTestError {
    message: String,
}

impl ServerTestError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ServerTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServerTestError {}

#[derive(Clone, Debug)]
enum ReaderTerminal {
    Eof,
    Failure(String),
}

#[derive(Debug, Default)]
struct InboxState {
    lines: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    terminal: Option<ReaderTerminal>,
}

#[derive(Debug, Default)]
struct EventInbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

impl EventInbox {
    fn push_line(&self, line: Vec<u8>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.terminal.is_some() {
            return;
        }
        if state.lines.len() >= MAX_QUEUED_LINES
            || state.queued_bytes.saturating_add(line.len()) > MAX_QUEUED_BYTES
        {
            state.lines.clear();
            state.queued_bytes = 0;
            state.terminal = Some(ReaderTerminal::Failure(format!(
                "server event inbox exceeded {MAX_QUEUED_LINES} lines or {MAX_QUEUED_BYTES} bytes"
            )));
            self.ready.notify_all();
            return;
        }
        state.queued_bytes += line.len();
        state.lines.push_back(line);
        self.ready.notify_one();
    }

    fn finish(&self, terminal: ReaderTerminal) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
        }
        self.ready.notify_all();
    }

    fn fail(&self, message: impl Into<String>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !matches!(state.terminal, Some(ReaderTerminal::Failure(_))) {
            state.terminal = Some(ReaderTerminal::Failure(message.into()));
        }
        self.ready.notify_all();
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, ReaderReceiveError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut state = self
            .state
            .lock()
            .map_err(|_| ReaderReceiveError::Failure("server event inbox lock poisoned".into()))?;

        loop {
            if let Some(ReaderTerminal::Failure(message)) = state.terminal.clone() {
                return Err(ReaderReceiveError::Failure(message));
            }
            if let Some(line) = state.lines.pop_front() {
                state.queued_bytes = state.queued_bytes.saturating_sub(line.len());
                return Ok(line);
            }
            if let Some(terminal) = state.terminal.clone() {
                return Err(match terminal {
                    ReaderTerminal::Eof => ReaderReceiveError::Eof,
                    ReaderTerminal::Failure(message) => ReaderReceiveError::Failure(message),
                });
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(ReaderReceiveError::Timeout);
            }
            let (next_state, wait_result) = self
                .ready
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .map_err(|_| {
                    ReaderReceiveError::Failure("server event inbox lock poisoned".into())
                })?;
            state = next_state;
            if wait_result.timed_out() && state.lines.is_empty() && state.terminal.is_none() {
                return Err(ReaderReceiveError::Timeout);
            }
        }
    }

    fn failure_message(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| match &state.terminal {
                Some(ReaderTerminal::Failure(message)) => Some(message.clone()),
                Some(ReaderTerminal::Eof) | None => None,
            })
    }
}

#[derive(Debug)]
enum ReaderReceiveError {
    Timeout,
    Eof,
    Failure(String),
}

#[derive(Debug, Default)]
struct CaptureBuffer {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl CaptureBuffer {
    fn extend(&mut self, chunk: &[u8]) {
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(self.bytes.len());
        let copied = remaining.min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..copied]);
        self.overflowed |= copied != chunk.len();
    }
}

#[derive(Debug, Default)]
struct RecentTranscript {
    lines: VecDeque<String>,
}

impl RecentTranscript {
    fn record(&mut self, line: &[u8]) {
        let original = String::from_utf8_lossy(line);
        let trimmed = original.trim_end_matches(['\r', '\n']);
        let mut rendered = trimmed
            .chars()
            .take(MAX_TRANSCRIPT_LINE_CHARS)
            .collect::<String>();
        if trimmed.chars().count() > MAX_TRANSCRIPT_LINE_CHARS {
            rendered.push_str("...");
        }
        if self.lines.len() == MAX_TRANSCRIPT_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(rendered);
    }

    fn render(&self) -> String {
        if self.lines.is_empty() {
            "<none>".to_string()
        } else {
            self.lines
                .iter()
                .enumerate()
                .map(|(index, line)| format!("  {:02}: {line}", index + 1))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

#[derive(Debug)]
struct PendingEvent {
    value: Value,
    encoded_len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReaderKind {
    Stdout,
    Stderr,
}

struct ReaderCompletion {
    kind: ReaderKind,
    sender: mpsc::Sender<ReaderKind>,
}

impl Drop for ReaderCompletion {
    fn drop(&mut self) {
        let _ = self.sender.send(self.kind);
    }
}

struct SpawnedChildGuard {
    child: Option<Child>,
    process_group_id: u32,
    process_job: Option<ProcessJob>,
}

impl SpawnedChildGuard {
    fn new(child: Child, process_job: ProcessJob) -> Self {
        let process_group_id = child.id();
        Self {
            child: Some(child),
            process_group_id,
            process_job: Some(process_job),
        }
    }

    fn take(&mut self) -> (Child, ProcessJob) {
        (
            self.child.take().expect("spawned child must be available"),
            self.process_job
                .take()
                .expect("spawned process job must be available"),
        )
    }
}

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = signal_process_group(self.process_group_id, TERMINATE_SIGNAL);
            thread::sleep(Duration::from_millis(20));
            let _ = signal_process_group(self.process_group_id, KILL_SIGNAL);
            if let Some(process_job) = self.process_job.as_ref() {
                let _ = process_job.terminate(1);
            }
            let _ = child.kill();
            let _ = wait_child_retry(child);
        }
    }
}

pub struct ServerTestClient {
    child: Option<Child>,
    status: Option<ExitStatus>,
    process_group_id: Option<u32>,
    process_job: Option<ProcessJob>,
    stdin: Option<ChildStdin>,
    inbox: Arc<EventInbox>,
    pending: VecDeque<PendingEvent>,
    pending_bytes: usize,
    transcript: Arc<Mutex<RecentTranscript>>,
    stdout_capture: Arc<Mutex<CaptureBuffer>>,
    stderr_capture: Arc<Mutex<CaptureBuffer>>,
    stop_readers: Arc<AtomicBool>,
    reader_done: mpsc::Receiver<ReaderKind>,
    stdout_done: bool,
    stderr_done: bool,
    stdout_worker: Option<JoinHandle<()>>,
    stderr_worker: Option<JoinHandle<()>>,
    event_timeout: Duration,
    _home: Option<TempDir>,
}

impl ServerTestClient {
    pub fn spawn(command: &mut Command, home: Option<TempDir>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            command.process_group(0);
        }

        let (child, process_job) = ProcessJob::spawn(command)?;
        let mut guard = SpawnedChildGuard::new(child, process_job);
        let stdin = guard.child.as_mut().and_then(|child| child.stdin.take());
        let stdout = guard
            .child
            .as_mut()
            .and_then(|child| child.stdout.take())
            .ok_or_else(|| io::Error::other("server child has no piped stdout"))?;
        let stderr = guard
            .child
            .as_mut()
            .and_then(|child| child.stderr.take())
            .ok_or_else(|| io::Error::other("server child has no piped stderr"))?;
        prepare_nonblocking(&stdout)?;
        prepare_nonblocking(&stderr)?;

        let inbox = Arc::new(EventInbox::default());
        let transcript = Arc::new(Mutex::new(RecentTranscript::default()));
        let stdout_capture = Arc::new(Mutex::new(CaptureBuffer::default()));
        let stderr_capture = Arc::new(Mutex::new(CaptureBuffer::default()));
        let stop_readers = Arc::new(AtomicBool::new(false));
        let (reader_done_tx, reader_done) = mpsc::channel();

        let stdout_worker = thread::Builder::new()
            .name("server-contract-stdout".into())
            .spawn({
                let inbox = Arc::clone(&inbox);
                let transcript = Arc::clone(&transcript);
                let capture = Arc::clone(&stdout_capture);
                let stop = Arc::clone(&stop_readers);
                let done = reader_done_tx.clone();
                move || read_stdout(stdout, inbox, transcript, capture, stop, done)
            })?;

        let stderr_worker = match thread::Builder::new()
            .name("server-contract-stderr".into())
            .spawn({
                let capture = Arc::clone(&stderr_capture);
                let stop = Arc::clone(&stop_readers);
                let inbox = Arc::clone(&inbox);
                move || read_stderr(stderr, capture, stop, inbox, reader_done_tx)
            }) {
            Ok(worker) => worker,
            Err(error) => {
                stop_readers.store(true, Ordering::Release);
                drop(guard);
                let _ = stdout_worker.join();
                return Err(error);
            }
        };

        let process_group_id = guard.process_group_id;
        let (child, process_job) = guard.take();
        Ok(Self {
            child: Some(child),
            status: None,
            process_group_id: Some(process_group_id),
            process_job: Some(process_job),
            stdin,
            inbox,
            pending: VecDeque::new(),
            pending_bytes: 0,
            transcript,
            stdout_capture,
            stderr_capture,
            stop_readers,
            reader_done,
            stdout_done: false,
            stderr_done: false,
            stdout_worker: Some(stdout_worker),
            stderr_worker: Some(stderr_worker),
            event_timeout: DEFAULT_EVENT_TIMEOUT,
            _home: home,
        })
    }

    pub fn id(&self) -> u32 {
        self.process_group_id
            .or_else(|| self.child.as_ref().map(Child::id))
            .expect("server process id is unavailable after cleanup")
    }

    pub fn stdin_mut(&mut self) -> &mut ChildStdin {
        self.stdin.as_mut().expect("server stdin is closed")
    }

    pub fn set_event_timeout(&mut self, timeout: Duration) {
        self.event_timeout = timeout;
    }

    pub fn close_stdin(&mut self) -> bool {
        self.stdin.take().is_some()
    }

    pub fn wait_for_event(&mut self, id: &str, event_name: &str) -> Result<Value, ServerTestError> {
        self.wait_for_event_matching(id, event_name, |_| true)
    }

    pub fn expect_event(&mut self, id: &str, event_name: &str) -> Value {
        self.wait_for_event(id, event_name)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn wait_for_event_matching(
        &mut self,
        id: &str,
        event_name: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<Value, ServerTestError> {
        let deadline = Instant::now()
            .checked_add(self.event_timeout)
            .unwrap_or_else(Instant::now);

        if let Some(message) = self.reader_failure_message() {
            return Err(self.diagnostic_error(format!(
                "server reader failed before {id}/{event_name}: {message}"
            )));
        }
        if let Some(result) = self.scan_pending(id, event_name, &predicate) {
            if result.is_ok()
                && let Some(message) = self.reader_failure_message()
            {
                return Err(self.diagnostic_error(format!(
                    "server reader failed before {id}/{event_name}: {message}"
                )));
            }
            return result;
        }

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(self.diagnostic_error(format!(
                    "timed out after {:?} waiting for {id}/{event_name}",
                    self.event_timeout
                )));
            }
            let event = match self.receive_protocol_event(deadline.saturating_duration_since(now)) {
                Ok(event) => event,
                Err(ReaderReceiveError::Timeout) => {
                    return Err(self.diagnostic_error(format!(
                        "timed out after {:?} waiting for {id}/{event_name}",
                        self.event_timeout
                    )));
                }
                Err(ReaderReceiveError::Eof) => {
                    return Err(self.diagnostic_error(format!(
                        "server stdout ended before {id}/{event_name}"
                    )));
                }
                Err(ReaderReceiveError::Failure(message)) => {
                    return Err(self.diagnostic_error(format!(
                        "server stdout reader failed before {id}/{event_name}: {message}"
                    )));
                }
            };

            match classify_event(&event, id, event_name, &predicate) {
                EventDecision::Match => {
                    if let Some(message) = self.reader_failure_message() {
                        return Err(self.diagnostic_error(format!(
                            "server reader failed before {id}/{event_name}: {message}"
                        )));
                    }
                    return Ok(event);
                }
                EventDecision::Impossible(reason) => {
                    return Err(self.diagnostic_error(reason));
                }
                EventDecision::Consume => {}
                EventDecision::Defer => self.defer_event(event)?,
            }
        }
    }

    pub fn expect_event_matching(
        &mut self,
        id: &str,
        event_name: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        self.wait_for_event_matching(id, event_name, predicate)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn expect_next_for_id(&mut self, id: &str) -> Value {
        self.wait_until_matching(&format!("next event for {id}"), |event| event["id"] == id)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn drain_events_until(
        &mut self,
        context: &str,
        predicate: impl FnMut(&Value) -> bool,
    ) -> Vec<Value> {
        self.collect_until(context, predicate)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn drain_events_until_matching(
        &mut self,
        context: &str,
        predicate: impl FnMut(&Value) -> bool,
    ) -> Vec<Value> {
        self.drain_events_until(context, predicate)
    }

    pub fn drain_events_until_event(&mut self, id: &str, event_name: &str) -> Vec<Value> {
        self.drain_events_until_protocol_event(id, event_name, false)
    }

    pub fn drain_events_until_event_or_error(&mut self, id: &str, event_name: &str) -> Vec<Value> {
        self.drain_events_until_protocol_event(id, event_name, true)
    }

    pub fn shutdown(&mut self) -> io::Result<ExitStatus> {
        self.shutdown_with_grace(DROP_GRACE_TIMEOUT)
    }

    pub fn wait_with_output(self) -> io::Result<Output> {
        self.wait_with_output_timeout(DEFAULT_EXIT_TIMEOUT)
    }

    pub fn wait_with_output_timeout(mut self, timeout: Duration) -> io::Result<Output> {
        let exited_before_deadline = self.close_and_wait(timeout)?;
        let status = self.status.ok_or_else(|| {
            io::Error::other("server child cleanup completed without an exit status")
        })?;
        let stdout = capture_bytes(&self.stdout_capture)?;
        let stderr = capture_bytes(&self.stderr_capture)?;
        if !exited_before_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "server child did not exit within {timeout:?}; status={status}; recent server output:\n{}\nstderr={}",
                    self.recent_transcript(),
                    String::from_utf8_lossy(&tail_bytes(&stderr, MAX_DIAGNOSTIC_STDERR_BYTES))
                ),
            ));
        }
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    fn wait_until_matching(
        &mut self,
        context: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<Value, ServerTestError> {
        let deadline = Instant::now()
            .checked_add(self.event_timeout)
            .unwrap_or_else(Instant::now);
        if let Some(message) = self.reader_failure_message() {
            return Err(
                self.diagnostic_error(format!("server reader failed before {context}: {message}"))
            );
        }
        if let Some(index) = self
            .pending
            .iter()
            .position(|event| predicate(&event.value))
        {
            let event = self.remove_pending(index);
            if let Some(message) = self.reader_failure_message() {
                return Err(self.diagnostic_error(format!(
                    "server reader failed before {context}: {message}"
                )));
            }
            return Ok(event);
        }
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(self.diagnostic_error(format!(
                    "timed out after {:?} waiting for {context}",
                    self.event_timeout
                )));
            }
            let event = match self.receive_protocol_event(deadline.saturating_duration_since(now)) {
                Ok(event) => event,
                Err(ReaderReceiveError::Timeout) => {
                    return Err(self.diagnostic_error(format!(
                        "timed out after {:?} waiting for {context}",
                        self.event_timeout
                    )));
                }
                Err(ReaderReceiveError::Eof) => {
                    return Err(
                        self.diagnostic_error(format!("server stdout ended before {context}"))
                    );
                }
                Err(ReaderReceiveError::Failure(message)) => {
                    return Err(self.diagnostic_error(format!(
                        "server stdout reader failed before {context}: {message}"
                    )));
                }
            };
            if predicate(&event) {
                if let Some(message) = self.reader_failure_message() {
                    return Err(self.diagnostic_error(format!(
                        "server reader failed before {context}: {message}"
                    )));
                }
                return Ok(event);
            }
            self.defer_event(event)?;
        }
    }

    fn collect_until(
        &mut self,
        context: &str,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Result<Vec<Value>, ServerTestError> {
        let deadline = Instant::now()
            .checked_add(self.event_timeout)
            .unwrap_or_else(Instant::now);
        let mut events = Vec::new();
        loop {
            if let Some(message) = self.reader_failure_message() {
                return Err(self.diagnostic_error(format!(
                    "server reader failed before {context}: {message}"
                )));
            }
            let event = if self.pending.is_empty() {
                let now = Instant::now();
                if now >= deadline {
                    return Err(self.diagnostic_error(format!(
                        "timed out after {:?} waiting for {context}",
                        self.event_timeout
                    )));
                }
                match self.receive_protocol_event(deadline.saturating_duration_since(now)) {
                    Ok(event) => event,
                    Err(ReaderReceiveError::Timeout) => {
                        return Err(self.diagnostic_error(format!(
                            "timed out after {:?} waiting for {context}",
                            self.event_timeout
                        )));
                    }
                    Err(ReaderReceiveError::Eof) => {
                        return Err(
                            self.diagnostic_error(format!("server stdout ended before {context}"))
                        );
                    }
                    Err(ReaderReceiveError::Failure(message)) => {
                        return Err(self.diagnostic_error(format!(
                            "server stdout reader failed before {context}: {message}"
                        )));
                    }
                }
            } else {
                self.remove_pending(0)
            };
            let done = predicate(&event);
            events.push(event);
            if done {
                self.ensure_reader_healthy(context)?;
                return Ok(events);
            }
        }
    }

    fn drain_events_until_protocol_event(
        &mut self,
        id: &str,
        event_name: &str,
        allow_matching_error: bool,
    ) -> Vec<Value> {
        self.try_drain_events_until_protocol_event(id, event_name, allow_matching_error)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn try_drain_events_until_protocol_event(
        &mut self,
        id: &str,
        event_name: &str,
        allow_matching_error: bool,
    ) -> Result<Vec<Value>, ServerTestError> {
        let context = format!("{id}/{event_name}");
        let deadline = Instant::now()
            .checked_add(self.event_timeout)
            .unwrap_or_else(Instant::now);
        let mut events = Vec::new();
        loop {
            let event = self.next_drained_event(deadline, &context)?;
            if event["id"] == id && allow_matching_error && event["event"] == "error" {
                events.push(event);
                self.ensure_reader_healthy(&context)?;
                return Ok(events);
            }
            match classify_event(&event, id, event_name, &|_| true) {
                EventDecision::Match => {
                    events.push(event);
                    self.ensure_reader_healthy(&context)?;
                    return Ok(events);
                }
                EventDecision::Impossible(reason) => {
                    return Err(self.diagnostic_error(reason));
                }
                EventDecision::Consume | EventDecision::Defer => events.push(event),
            }
        }
    }

    fn next_drained_event(
        &mut self,
        deadline: Instant,
        context: &str,
    ) -> Result<Value, ServerTestError> {
        if let Some(message) = self.reader_failure_message() {
            return Err(
                self.diagnostic_error(format!("server reader failed before {context}: {message}"))
            );
        }
        if !self.pending.is_empty() {
            return Ok(self.remove_pending(0));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(self.diagnostic_error(format!(
                "timed out after {:?} waiting for {context}",
                self.event_timeout
            )));
        }
        match self.receive_protocol_event(deadline.saturating_duration_since(now)) {
            Ok(event) => Ok(event),
            Err(ReaderReceiveError::Timeout) => Err(self.diagnostic_error(format!(
                "timed out after {:?} waiting for {context}",
                self.event_timeout
            ))),
            Err(ReaderReceiveError::Eof) => {
                Err(self.diagnostic_error(format!("server stdout ended before {context}")))
            }
            Err(ReaderReceiveError::Failure(message)) => Err(self.diagnostic_error(format!(
                "server stdout reader failed before {context}: {message}"
            ))),
        }
    }

    fn scan_pending(
        &mut self,
        id: &str,
        event_name: &str,
        predicate: &impl Fn(&Value) -> bool,
    ) -> Option<Result<Value, ServerTestError>> {
        let mut index = 0;
        while index < self.pending.len() {
            let decision = classify_event(&self.pending[index].value, id, event_name, predicate);
            match decision {
                EventDecision::Match => return Some(Ok(self.remove_pending(index))),
                EventDecision::Impossible(reason) => {
                    return Some(Err(self.diagnostic_error(reason)));
                }
                EventDecision::Consume => {
                    self.remove_pending(index);
                }
                EventDecision::Defer => index += 1,
            }
        }
        None
    }

    fn receive_protocol_event(&self, timeout: Duration) -> Result<Value, ReaderReceiveError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if let Some(message) = self.reader_failure_message() {
                return Err(ReaderReceiveError::Failure(message));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ReaderReceiveError::Timeout);
            }
            let line = self
                .inbox
                .recv_timeout(deadline.saturating_duration_since(now))?;
            if let Some(message) = self.reader_failure_message() {
                return Err(ReaderReceiveError::Failure(message));
            }
            let trimmed = trim_ascii_whitespace(&line);
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_slice::<Value>(trimmed) {
                return Ok(event);
            }
        }
    }

    fn defer_event(&mut self, event: Value) -> Result<(), ServerTestError> {
        let encoded_len = serde_json::to_vec(&event).map_or(0, |encoded| encoded.len());
        if self.pending.len() >= MAX_PENDING_EVENTS
            || self.pending_bytes.saturating_add(encoded_len) > MAX_PENDING_BYTES
        {
            return Err(self.diagnostic_error(format!(
                "unmatched server events exceeded {MAX_PENDING_EVENTS} events or {MAX_PENDING_BYTES} bytes"
            )));
        }
        self.pending_bytes += encoded_len;
        self.pending.push_back(PendingEvent {
            value: event,
            encoded_len,
        });
        Ok(())
    }

    fn remove_pending(&mut self, index: usize) -> Value {
        let event = self
            .pending
            .remove(index)
            .expect("pending event index must remain valid");
        self.pending_bytes = self.pending_bytes.saturating_sub(event.encoded_len);
        event.value
    }

    fn diagnostic_error(&mut self, reason: String) -> ServerTestError {
        let child_status = match self.leader_has_exited() {
            Ok(true) if self.status.is_some() => self.status.expect("checked status").to_string(),
            Ok(true) => "exited, awaiting reap".to_string(),
            Ok(false) => "still running".to_string(),
            Err(error) => format!("status unavailable: {error}"),
        };
        let transcript = self.recent_transcript();
        let stderr = capture_bytes_lossy(&self.stderr_capture);
        let stderr =
            String::from_utf8_lossy(&tail_bytes(&stderr, MAX_DIAGNOSTIC_STDERR_BYTES)).into_owned();
        ServerTestError::new(format!(
            "{reason}\nchild status: {child_status}\nrecent server output:\n{transcript}\nrecent server stderr:\n{}",
            if stderr.is_empty() { "<none>" } else { &stderr }
        ))
    }

    fn recent_transcript(&self) -> String {
        self.transcript
            .lock()
            .map(|transcript| transcript.render())
            .unwrap_or_else(|_| "<transcript lock poisoned>".to_string())
    }

    fn reader_failure_message(&self) -> Option<String> {
        self.inbox.failure_message()
    }

    fn ensure_reader_healthy(&mut self, context: &str) -> Result<(), ServerTestError> {
        if let Some(message) = self.reader_failure_message() {
            Err(self.diagnostic_error(format!("server reader failed before {context}: {message}")))
        } else {
            Ok(())
        }
    }

    fn shutdown_with_grace(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        self.close_and_wait(timeout)?;
        self.status.ok_or_else(|| {
            io::Error::other("server child cleanup completed without an exit status")
        })
    }

    fn close_and_wait(&mut self, timeout: Duration) -> io::Result<bool> {
        self.close_stdin();
        if self.status.is_some() {
            return self.finish_readers().map(|_| true);
        }
        let mut first_error = None;
        let exited_before_deadline = match self.wait_for_leader_exit(timeout) {
            Ok(exited) => exited,
            Err(error) => {
                first_error = Some(error);
                false
            }
        };
        if let Err(error) = self.cleanup_process_group(exited_before_deadline)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.finish_readers()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(exited_before_deadline)
        }
    }

    fn wait_for_leader_exit(&mut self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if self.leader_has_exited()? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn leader_has_exited(&mut self) -> io::Result<bool> {
        if let Some(status) = self.status {
            let _ = status;
            return Ok(true);
        }
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("server child is unavailable"))?;
        #[cfg(unix)]
        {
            child_exited_without_reaping(child.id())
        }
        #[cfg(not(unix))]
        {
            if let Some(status) = child.try_wait()? {
                self.status = Some(status);
                self.child.take();
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    fn cleanup_process_group(&mut self, leader_already_exited: bool) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let mut first_error = None;
        let mut leader_exited = leader_already_exited;
        if let Some(process_group_id) = self.process_group_id {
            if let Err(error) =
                signal_process_group_for_cleanup(process_group_id, TERMINATE_SIGNAL, leader_exited)
            {
                first_error = Some(error);
            }
            if leader_exited {
                thread::sleep(Duration::from_millis(20));
            } else {
                match self.wait_for_leader_exit(SIGNAL_GRACE_TIMEOUT) {
                    Ok(exited) => leader_exited = exited,
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            if let Err(error) =
                signal_process_group_for_cleanup(process_group_id, KILL_SIGNAL, leader_exited)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(process_job) = self.process_job.as_ref()
            && let Err(error) = process_job.terminate(1)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Some(status) = self.status {
            self.process_group_id.take();
            self.process_job.take();
            return if let Some(error) = first_error {
                Err(error)
            } else {
                Ok(status)
            };
        }

        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("server child is unavailable during cleanup"))?;
        let _ = child.kill();
        let status = wait_child_retry(child)?;
        self.status = Some(status);
        self.child.take();
        self.process_group_id.take();
        self.process_job.take();
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(status)
        }
    }

    fn finish_readers(&mut self) -> io::Result<()> {
        self.wait_for_reader_completion(Instant::now() + READER_EOF_TIMEOUT);
        if !self.stdout_done || !self.stderr_done {
            self.stop_readers.store(true, Ordering::Release);
            self.wait_for_reader_completion(Instant::now() + READER_STOP_TIMEOUT);
        }

        let mut first_error = None;
        if !self.stdout_done || !self.stderr_done {
            first_error = Some(io::Error::new(
                io::ErrorKind::TimedOut,
                "server output readers did not stop after process-group cleanup",
            ));
        }
        if let Some(worker) = self.stdout_worker.take()
            && worker.join().is_err()
        {
            first_error.get_or_insert_with(|| io::Error::other("server stdout reader panicked"));
        }
        if let Some(worker) = self.stderr_worker.take()
            && worker.join().is_err()
            && first_error.is_none()
        {
            first_error = Some(io::Error::other("server stderr reader panicked"));
        }
        if let Some(error) = first_error {
            Err(error)
        } else if let Some(message) = self.reader_failure_message() {
            Err(io::Error::other(message))
        } else {
            Ok(())
        }
    }

    fn wait_for_reader_completion(&mut self, deadline: Instant) {
        while !self.stdout_done || !self.stderr_done {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            match self
                .reader_done
                .recv_timeout(deadline.saturating_duration_since(now))
            {
                Ok(ReaderKind::Stdout) => self.stdout_done = true,
                Ok(ReaderKind::Stderr) => self.stderr_done = true,
                Err(_) => return,
            }
        }
    }
}

impl Drop for ServerTestClient {
    fn drop(&mut self) {
        let _ = self.shutdown_with_grace(DROP_GRACE_TIMEOUT);
    }
}

enum EventDecision {
    Match,
    Impossible(String),
    Consume,
    Defer,
}

fn classify_event(
    event: &Value,
    id: &str,
    event_name: &str,
    predicate: &impl Fn(&Value) -> bool,
) -> EventDecision {
    if event["id"] != id {
        return EventDecision::Defer;
    }
    if event["event"] == event_name {
        if predicate(event) {
            return EventDecision::Match;
        }
        if matches!(event_name, "turn_completed" | "error") {
            return EventDecision::Impossible(format!(
                "matching terminal event did not satisfy the expectation for {id}/{event_name}: {event}"
            ));
        }
        return EventDecision::Defer;
    }
    if event["event"] == "error" && event_name != "error" && event_name != "turn_completed" {
        return EventDecision::Impossible(format!(
            "server returned error before {id}/{event_name}: {}",
            event["message"].as_str().unwrap_or("<missing message>")
        ));
    }
    if event["event"] == "turn_completed" && turn_completion_precludes(event_name) {
        return EventDecision::Impossible(format!(
            "server completed the turn before {id}/{event_name}: {event}"
        ));
    }
    EventDecision::Consume
}

fn turn_completion_precludes(event_name: &str) -> bool {
    matches!(
        event_name,
        "turn_started"
            | "reasoning_delta"
            | "message_delta"
            | "item_started"
            | "item_message_delta"
            | "item_reasoning_delta"
            | "tool_requested"
            | "tool_completed"
            | "permission_request"
            | "mcp_elicitation_request"
            | "turn_plan_updated"
            | "workflow_result_available"
    )
}

fn read_stdout(
    mut stdout: impl Read,
    inbox: Arc<EventInbox>,
    transcript: Arc<Mutex<RecentTranscript>>,
    capture: Arc<Mutex<CaptureBuffer>>,
    stop: Arc<AtomicBool>,
    done: mpsc::Sender<ReaderKind>,
) {
    let _completion = ReaderCompletion {
        kind: ReaderKind::Stdout,
        sender: done,
    };
    let mut chunk = [0_u8; 8_192];
    let mut pending_line = Vec::new();
    loop {
        if stop.load(Ordering::Acquire) {
            if !pending_line.is_empty() {
                record_stdout_line(&pending_line, &inbox, &transcript);
            }
            inbox.finish(ReaderTerminal::Eof);
            return;
        }
        match stdout.read(&mut chunk) {
            Ok(0) => {
                if !pending_line.is_empty() {
                    record_stdout_line(&pending_line, &inbox, &transcript);
                }
                inbox.finish(ReaderTerminal::Eof);
                return;
            }
            Ok(read) => {
                let bytes = &chunk[..read];
                if let Ok(mut capture) = capture.lock() {
                    capture.extend(bytes);
                    if capture.overflowed {
                        inbox.fail(format!(
                            "server stdout exceeded {MAX_CAPTURE_BYTES} captured bytes"
                        ));
                    }
                } else {
                    inbox.fail("server stdout capture lock poisoned");
                }
                pending_line.extend_from_slice(bytes);
                while let Some(newline) = pending_line.iter().position(|byte| *byte == b'\n') {
                    let line = pending_line.drain(..=newline).collect::<Vec<_>>();
                    record_stdout_line(&line, &inbox, &transcript);
                }
                if pending_line.len() > MAX_PROTOCOL_LINE_BYTES {
                    inbox.fail(format!(
                        "server stdout line exceeded {MAX_PROTOCOL_LINE_BYTES} bytes"
                    ));
                    pending_line.clear();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(READER_IDLE_POLL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                inbox.fail(format!("server stdout read failed: {error}"));
                return;
            }
        }
    }
}

fn record_stdout_line(line: &[u8], inbox: &EventInbox, transcript: &Mutex<RecentTranscript>) {
    if let Ok(mut transcript) = transcript.lock() {
        transcript.record(line);
    }
    inbox.push_line(line.to_vec());
}

fn read_stderr(
    mut stderr: impl Read,
    capture: Arc<Mutex<CaptureBuffer>>,
    stop: Arc<AtomicBool>,
    inbox: Arc<EventInbox>,
    done: mpsc::Sender<ReaderKind>,
) {
    let _completion = ReaderCompletion {
        kind: ReaderKind::Stderr,
        sender: done,
    };
    let mut chunk = [0_u8; 8_192];
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match stderr.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => {
                if let Ok(mut capture) = capture.lock() {
                    capture.extend(&chunk[..read]);
                    if capture.overflowed {
                        inbox.fail(format!(
                            "server stderr exceeded {MAX_CAPTURE_BYTES} captured bytes"
                        ));
                    }
                } else {
                    inbox.fail("server stderr capture lock poisoned");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(READER_IDLE_POLL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                inbox.fail(format!("server stderr read failed: {error}"));
                return;
            }
        }
    }
}

fn capture_bytes(capture: &Mutex<CaptureBuffer>) -> io::Result<Vec<u8>> {
    let capture = capture
        .lock()
        .map_err(|_| io::Error::other("server output capture lock poisoned"))?;
    if capture.overflowed {
        return Err(io::Error::other(format!(
            "server output exceeded {MAX_CAPTURE_BYTES} captured bytes"
        )));
    }
    Ok(capture.bytes.clone())
}

fn capture_bytes_lossy(capture: &Mutex<CaptureBuffer>) -> Vec<u8> {
    capture
        .lock()
        .map(|capture| capture.bytes.clone())
        .unwrap_or_default()
}

fn tail_bytes(bytes: &[u8], max_len: usize) -> Vec<u8> {
    bytes[bytes.len().saturating_sub(max_len)..].to_vec()
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

#[cfg(unix)]
fn prepare_nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
    let descriptor = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_nonblocking(_reader: &impl Read) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn child_exited_without_reaping(pid: u32) -> io::Result<bool> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            let info = unsafe { info.assume_init() };
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn wait_child_retry(child: &mut Child) -> io::Result<ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn signal_process_group(pid: u32, signal: i32) -> io::Result<()> {
    #[cfg(unix)]
    unsafe {
        if libc::kill(-(pid as i32), signal) == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Ok(())
    }
}

fn signal_process_group_for_cleanup(pid: u32, signal: i32, leader_exited: bool) -> io::Result<()> {
    match signal_process_group(pid, signal) {
        Err(error) if empty_macos_process_group_permission_error(&error, pid, leader_exited) => {
            Ok(())
        }
        result => result,
    }
}

#[cfg(target_os = "macos")]
fn empty_macos_process_group_permission_error(
    error: &io::Error,
    process_group_id: u32,
    leader_exited: bool,
) -> bool {
    if !leader_exited || error.kind() != io::ErrorKind::PermissionDenied {
        return false;
    }

    let mut members = [0 as libc::pid_t; 2];
    let member_count = unsafe {
        libc::proc_listpgrppids(
            process_group_id as libc::pid_t,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as libc::c_int,
        )
    };
    member_count == 1 && members[0] == process_group_id as libc::pid_t
}

#[cfg(not(target_os = "macos"))]
fn empty_macos_process_group_permission_error(
    _error: &io::Error,
    _process_group_id: u32,
    _leader_exited: bool,
) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    struct WouldBlockReader;

    impl Read for WouldBlockReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    fn scripted_client(script: &str) -> ServerTestClient {
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        ServerTestClient::spawn(&mut command, None).expect("spawn scripted server")
    }

    #[test]
    fn missing_event_deadline_reports_recent_protocol_and_noise() {
        let mut client = scripted_client(
            "printf 'startup noise\\n{\"id\":\"turn\",\"event\":\"turn_started\"}\\n'; sleep 5",
        );
        client.set_event_timeout(Duration::from_millis(500));

        let started = Instant::now();
        let error = client
            .wait_for_event("turn", "permission_request")
            .expect_err("missing event must fail");

        assert!(started.elapsed() < Duration::from_secs(1), "{error}");
        let message = error.to_string();
        assert!(message.contains("timed out after"), "{message}");
        assert!(message.contains("startup noise"), "{message}");
        assert!(message.contains("turn_started"), "{message}");
    }

    #[test]
    fn completed_turn_fails_impossible_permission_expectation_before_deadline() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"tool_completed\",\"tool\":\"bash\",\"status\":\"failed\",\"error\":\"proxy start: Operation not permitted\"}\\n{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"success\"}\\n'; sleep 5",
        );
        client.set_event_timeout(Duration::from_secs(2));

        let started = Instant::now();
        let error = client
            .wait_for_event("turn", "permission_request")
            .expect_err("completed turn must make permission request impossible");

        assert!(started.elapsed() < Duration::from_millis(500), "{error}");
        let message = error.to_string();
        assert!(message.contains("completed the turn"), "{message}");
        assert!(message.contains("Operation not permitted"), "{message}");
    }

    #[test]
    fn failed_sibling_tool_does_not_preclude_later_permission_request() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"tool_completed\",\"tool\":\"first\",\"status\":\"failed\"}\\n{\"id\":\"turn\",\"event\":\"permission_request\",\"requestId\":\"permission-2\"}\\n'",
        );

        let permission = client.expect_event("turn", "permission_request");

        assert_eq!(permission["requestId"], "permission-2");
    }

    #[test]
    fn cancellation_error_can_precede_cancelled_turn_completion() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"error\",\"message\":\"turn cancelled\"}\\n{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"cancelled\"}\\n'",
        );

        let completed = client.expect_event("turn", "turn_completed");

        assert_eq!(completed["status"], "cancelled");
    }

    #[test]
    fn matching_error_makes_nonterminal_expectation_impossible() {
        let mut client = scripted_client(
            "printf '{\"id\":\"read\",\"event\":\"error\",\"message\":\"unknown thread\"}\\n'; sleep 5",
        );

        let error = client
            .wait_for_event("read", "thread_read")
            .expect_err("matching error must fail immediately");

        assert!(error.to_string().contains("unknown thread"), "{error}");
    }

    #[test]
    fn request_error_before_turn_start_fails_fast() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"error\",\"message\":\"unknown thread\"}\\n'; sleep 5",
        );
        client.set_event_timeout(Duration::from_secs(2));

        let started = Instant::now();
        let error = client
            .wait_for_event("turn", "turn_started")
            .expect_err("request error must preclude turn start");

        assert!(started.elapsed() < Duration::from_millis(500), "{error}");
        assert!(error.to_string().contains("unknown thread"), "{error}");
    }

    #[test]
    fn typed_event_drain_fails_fast_on_matching_error() {
        let mut client = scripted_client(
            "printf '{\"id\":\"read\",\"event\":\"error\",\"message\":\"unknown thread\"}\\n'; sleep 5",
        );

        let error = client
            .try_drain_events_until_protocol_event("read", "thread_read", false)
            .expect_err("typed drain must preserve matching-error semantics");

        assert!(error.to_string().contains("unknown thread"), "{error}");
    }

    #[test]
    fn nonfatal_turn_error_does_not_preclude_turn_completion() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"error\",\"message\":\"session_start hook failed: boom\"}\\n{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"success\"}\\n'",
        );

        let completed = client.expect_event("turn", "turn_completed");

        assert_eq!(completed["status"], "success");
    }

    #[test]
    fn deferred_turn_warning_does_not_preclude_completion() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"error\",\"message\":\"plan warning\"}\\n{\"id\":\"other\",\"event\":\"done\"}\\n{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"success\"}\\n'",
        );

        assert_eq!(client.expect_event("other", "done")["id"], "other");
        assert_eq!(
            client.expect_event("turn", "turn_completed")["status"],
            "success"
        );
    }

    #[test]
    fn turn_completion_does_not_preclude_late_item_cleanup() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"cancelled\"}\\n{\"id\":\"turn\",\"event\":\"item_completed\",\"item\":{\"id\":\"reasoning-1\"}}\\n'",
        );

        let item = client.expect_event("turn", "item_completed");

        assert_eq!(item["item"]["id"], "reasoning-1");
    }

    #[test]
    fn terminal_predicate_mismatch_fails_before_deadline() {
        let mut client = scripted_client(
            "printf '{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"failed\"}\\n'; sleep 5",
        );
        client.set_event_timeout(Duration::from_secs(2));

        let started = Instant::now();
        let error = client
            .wait_for_event_matching("turn", "turn_completed", |event| {
                event["status"] == "success"
            })
            .expect_err("terminal predicate mismatch must be impossible");

        assert!(started.elapsed() < Duration::from_millis(500), "{error}");
        assert!(error.to_string().contains("did not satisfy"), "{error}");
    }

    #[test]
    fn exit_timeout_reports_recent_protocol_transcript() {
        let client = scripted_client(
            "trap '' TERM; printf '{\"id\":\"turn\",\"event\":\"turn_started\"}\\n'; while :; do sleep 1; done",
        );

        let error = client
            .wait_with_output_timeout(Duration::from_millis(50))
            .expect_err("stubborn server must time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("turn_started"), "{error}");
    }

    #[test]
    fn sticky_reader_failure_precludes_an_already_queued_match() {
        let mut client = scripted_client("sleep 5");
        client.inbox.push_line(
            b"{\"id\":\"turn\",\"event\":\"turn_completed\",\"status\":\"success\"}\n".to_vec(),
        );
        client.inbox.fail("synthetic reader overflow");

        let error = client
            .wait_for_event("turn", "turn_completed")
            .expect_err("reader failure must remain sticky");

        assert!(error.to_string().contains("synthetic reader overflow"));
    }

    #[test]
    fn reader_failure_wakes_an_active_event_wait() {
        let mut client = scripted_client("sleep 5");
        client.set_event_timeout(Duration::from_secs(2));
        let inbox = Arc::clone(&client.inbox);
        let notifier = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            inbox.fail("synthetic stderr overflow");
        });

        let started = Instant::now();
        let error = client
            .wait_for_event("turn", "turn_completed")
            .expect_err("reader failure must wake the waiter");
        notifier.join().expect("failure notifier");

        assert!(started.elapsed() < Duration::from_millis(500), "{error}");
        assert!(error.to_string().contains("synthetic stderr overflow"));
    }

    #[test]
    fn stdout_reader_stop_is_a_clean_terminal() {
        let inbox = Arc::new(EventInbox::default());
        let transcript = Arc::new(Mutex::new(RecentTranscript::default()));
        let capture = Arc::new(Mutex::new(CaptureBuffer::default()));
        let stop = Arc::new(AtomicBool::new(true));
        let (done_tx, done_rx) = mpsc::channel();

        read_stdout(
            WouldBlockReader,
            Arc::clone(&inbox),
            transcript,
            capture,
            stop,
            done_tx,
        );

        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Ok(ReaderKind::Stdout)
        );
        assert!(inbox.failure_message().is_none());
        assert!(matches!(
            inbox.recv_timeout(Duration::ZERO),
            Err(ReaderReceiveError::Eof)
        ));
    }

    #[test]
    fn unmatched_events_remain_available_for_later_expectations() {
        let mut client = scripted_client(
            "printf '{\"id\":\"second\",\"event\":\"done\"}\\n{\"id\":\"first\",\"event\":\"done\"}\\n'",
        );

        assert_eq!(client.expect_event("first", "done")["id"], "first");
        assert_eq!(client.expect_event("second", "done")["id"], "second");
    }

    #[test]
    fn shutdown_is_idempotent_after_graceful_stdin_close() {
        let mut client = scripted_client("cat >/dev/null");

        let first = client.shutdown().expect("first shutdown");
        let second = client.shutdown().expect("second shutdown");

        assert!(first.success(), "{first}");
        assert_eq!(second, first);
    }

    #[test]
    #[cfg(unix)]
    fn drop_forcibly_reaps_an_uncooperative_process_group() {
        let mut client = scripted_client(
            "trap '' TERM; (trap '' TERM; while :; do sleep 1; done) & child=$!; printf '{\"id\":\"ready\",\"event\":\"ready\",\"childPid\":%s}\\n' \"$child\"; while :; do sleep 1; done",
        );
        let parent_pid = client.id();
        let ready = client.expect_event("ready", "ready");
        let child_pid = ready["childPid"].as_u64().expect("child pid") as u32;

        let started = Instant::now();
        drop(client);

        assert!(started.elapsed() < Duration::from_secs(3));
        assert_process_gone(parent_pid);
        assert_process_gone(child_pid);
    }

    #[test]
    #[cfg(unix)]
    fn drop_reaps_descendant_after_leader_exits_with_inherited_pipes() {
        let mut client = scripted_client(
            "(trap '' TERM; while :; do sleep 1; done) & child=$!; printf '{\"id\":\"ready\",\"event\":\"ready\",\"childPid\":%s}\\n' \"$child\"",
        );
        let parent_pid = client.id();
        let ready = client.expect_event("ready", "ready");
        let child_pid = ready["childPid"].as_u64().expect("child pid") as u32;
        assert!(
            client
                .wait_for_leader_exit(Duration::from_secs(1))
                .expect("observe leader exit without reaping")
        );

        drop(client);

        assert_process_gone(parent_pid);
        assert_process_gone(child_pid);
    }

    #[test]
    #[cfg(unix)]
    fn explicit_shutdown_succeeds_after_stopping_inherited_pipe_reader() {
        let mut client = scripted_client(
            "(trap '' TERM; while :; do sleep 1; done) & child=$!; printf '{\"id\":\"ready\",\"event\":\"ready\",\"childPid\":%s}\\n' \"$child\"",
        );
        let ready = client.expect_event("ready", "ready");
        let child_pid = ready["childPid"].as_u64().expect("child pid") as u32;
        assert!(
            client
                .wait_for_leader_exit(Duration::from_secs(1))
                .expect("observe leader exit without reaping")
        );

        let status = client
            .shutdown()
            .expect("bounded reader stop is normal cleanup");

        assert!(status.success(), "{status}");
        assert_process_gone(child_pid);
    }

    #[test]
    #[cfg(unix)]
    fn drop_reaps_descendant_after_leader_exits_without_open_pipes() {
        let mut client = scripted_client(
            "(trap '' TERM; exec >/dev/null 2>&1; while :; do sleep 1; done) & child=$!; printf '{\"id\":\"ready\",\"event\":\"ready\",\"childPid\":%s}\\n' \"$child\"",
        );
        let parent_pid = client.id();
        let ready = client.expect_event("ready", "ready");
        let child_pid = ready["childPid"].as_u64().expect("child pid") as u32;
        assert!(
            client
                .wait_for_leader_exit(Duration::from_secs(1))
                .expect("observe leader exit without reaping")
        );

        drop(client);

        assert_process_gone(parent_pid);
        assert_process_gone(child_pid);
    }

    #[cfg(unix)]
    fn assert_process_gone(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while process_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(pid), "process {pid} survived cleanup");
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}
