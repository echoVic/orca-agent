//! Keeps stderr off the screen while the TUI draws it.
//!
//! The renderer redraws only the cells it changed, so a line written to stderr
//! while the screen is up stays painted over the frame: the runtime reports
//! some failures with `eprintln!`, and one written after Esc sat on the
//! composer. While the screen is up, fd 2 is a pipe instead, and each line it
//! carries becomes a notice in the transcript, once. When the screen goes,
//! fd 2 is the terminal again, and whatever still comes down the pipe (from a
//! child that inherited it) passes through to the terminal.

use crate::channels::TuiEventSender;

pub(crate) struct StderrCapture {
    #[cfg(unix)]
    capture: Option<platform::Capture>,
}

impl StderrCapture {
    /// Captures stderr when it is the terminal: a redirected stderr cannot
    /// draw over the screen, and keeps what is written to it. Best effort; if
    /// the capture cannot start, stderr stays as it was.
    pub(crate) fn start(event_tx: TuiEventSender) -> Self {
        use std::io::IsTerminal as _;

        Self::start_with(event_tx, std::io::stderr().is_terminal())
    }

    #[cfg(unix)]
    fn start_with(event_tx: TuiEventSender, stderr_is_terminal: bool) -> Self {
        Self {
            capture: stderr_is_terminal
                .then(|| platform::Capture::start(event_tx).ok())
                .flatten(),
        }
    }

    #[cfg(not(unix))]
    fn start_with(_event_tx: TuiEventSender, _stderr_is_terminal: bool) -> Self {
        Self {}
    }

    /// Puts the terminal back on fd 2. Calling it again does nothing.
    pub(crate) fn stop(&mut self) {
        #[cfg(unix)]
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
    }
}

impl Drop for StderrCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Puts the terminal back on fd 2 before a panic that ends the screen is
/// reported, so the report reaches the terminal rather than the pipe.
pub(crate) fn restore_for_panic() {
    #[cfg(unix)]
    platform::restore();
}

#[cfg(unix)]
mod platform {
    use std::collections::HashSet;
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

    use crossbeam_channel::SendTimeoutError;

    use crate::channels::TuiEventSender;
    use crate::protocol::TuiEvent;

    /// How long a line waits for room in a full event mailbox before it is
    /// dropped. The reader has to keep draining the pipe: once the pipe is
    /// full, every write to stderr blocks.
    const FORWARD_TIMEOUT: Duration = Duration::from_millis(250);
    /// The most of one line a notice shows.
    const MAX_NOTICE_CHARS: usize = 2000;
    /// How many distinct lines are remembered to show each only once.
    const MAX_REMEMBERED_LINES: usize = 256;

    /// The saved terminal fd while the pipe stands in for it, so that a panic
    /// can put it back; -1 otherwise.
    static TERMINAL_STDERR: AtomicI32 = AtomicI32::new(-1);

    pub(super) struct Capture {
        /// The terminal as fd 2 had it; the reader writes to a duplicate.
        _terminal: OwnedFd,
        /// Where lines go while the screen is up. Taken when it goes.
        transcript: Arc<Mutex<Option<TuiEventSender>>>,
    }

    impl Capture {
        pub(super) fn start(event_tx: TuiEventSender) -> io::Result<Self> {
            let terminal = io::stderr().as_fd().try_clone_to_owned()?;
            let to_terminal = File::from(terminal.try_clone()?);
            let (reader, writer) = io::pipe()?;
            let transcript = Arc::new(Mutex::new(Some(event_tx)));
            let reader_transcript = Arc::clone(&transcript);
            thread::Builder::new()
                .name("orca-stderr".to_string())
                .spawn(move || forward_lines(reader, &reader_transcript, to_terminal))?;
            // SAFETY: dup2 swaps what fd 2 refers to in one step; the pipe's
            // write end stays open as fd 2 after `writer` is dropped.
            if unsafe { libc::dup2(writer.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
                return Err(io::Error::last_os_error());
            }
            TERMINAL_STDERR.store(terminal.as_raw_fd(), Ordering::SeqCst);
            Ok(Self {
                _terminal: terminal,
                transcript,
            })
        }

        pub(super) fn stop(self) {
            restore();
            self.transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
        }
    }

    pub(super) fn restore() {
        let terminal = TERMINAL_STDERR.swap(-1, Ordering::SeqCst);
        if terminal >= 0 {
            // SAFETY: `terminal` is the saved fd, which the capture keeps open
            // until after it has been put back.
            unsafe { libc::dup2(terminal, libc::STDERR_FILENO) };
        }
    }

    fn forward_lines(
        reader: io::PipeReader,
        transcript: &Mutex<Option<TuiEventSender>>,
        mut terminal: File,
    ) {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        let mut shown = HashSet::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
            let event_tx = transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let Some(event_tx) = event_tx else {
                let _ = terminal.write_all(&line);
                continue;
            };
            let Some(notice) = notice_text(&line) else {
                continue;
            };
            if shown.len() >= MAX_REMEMBERED_LINES {
                shown.clear();
            }
            if !shown.insert(notice.clone()) {
                continue;
            }
            if let Err(SendTimeoutError::Disconnected(_)) =
                event_tx.send_timeout(TuiEvent::Notice(notice), FORWARD_TIMEOUT)
            {
                // The transcript is gone before the screen was given back.
                let _ = terminal.write_all(&line);
            }
        }
    }

    /// A line as its notice shows it: what a terminal would have printed for
    /// it, trimmed and capped; nothing for a blank line.
    pub(super) fn notice_text(line: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(line);
        let printed = crate::terminal_output::printable_output(&text);
        let trimmed = printed.trim();
        (!trimmed.is_empty()).then(|| trimmed.chars().take(MAX_NOTICE_CHARS).collect())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::protocol::TuiEvent;

    const CHILD_ENV: &str = "ORCA_TUI_STDERR_CAPTURE_CHILD";

    /// Runs in a child process, whose fd 2 is a pipe to the parent test, so
    /// that taking fd 2 over hides nothing of this test run's own output.
    #[test]
    fn stderr_capture_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let (event_tx, event_rx) = crate::channels::tui_event_channel();
        let mut capture = StderrCapture::start_with(event_tx, true);
        write_to_fd_2(b"orca: failed to stop foreground task tree\n");
        write_to_fd_2(b"\x1b[31mcoloured\x1b[0m line\r\n");
        write_to_fd_2(b"orca: failed to stop foreground task tree\n");
        write_to_fd_2(b"\n");
        let mut notices = Vec::new();
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(1)) {
            notices.push(event);
        }
        // Put the terminal back before asserting, so a failure is reported.
        capture.stop();
        write_to_fd_2(b"after the screen\n");
        let notices = notices
            .into_iter()
            .map(|event| match event {
                TuiEvent::Notice(text) => text,
                other => panic!("unexpected event {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notices,
            ["orca: failed to stop foreground task tree", "coloured line"]
        );
    }

    #[test]
    fn stderr_lines_become_notices_while_captured_and_reach_the_terminal_after() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stderr_capture::tests::stderr_capture_child",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("run the capturing child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "child failed: {stderr}");
        assert!(stderr.contains("after the screen"), "{stderr}");
        assert!(!stderr.contains("foreground task tree"), "{stderr}");
        assert!(!stderr.contains("coloured"), "{stderr}");
    }

    #[test]
    fn a_notice_shows_what_a_terminal_would_print() {
        assert_eq!(
            platform::notice_text(b"\x1b]0;title\x07warn\tx\r\n").as_deref(),
            Some("warn    x")
        );
        assert_eq!(platform::notice_text(b"   \n"), None);
    }

    fn write_to_fd_2(bytes: &[u8]) {
        // Straight to fd 2, as a write from anywhere in the process lands.
        let written =
            unsafe { libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(written, bytes.len() as isize);
    }
}
