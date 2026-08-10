#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PROMPT: &str = "typed TUI PTY submit";
const ASSISTANT_SENTINEL: &str = "Mock runtime completed the headless harness contract.";

#[test]
fn tui_submit_renders_and_restores_the_terminal() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "TUI did not render the typed assistant terminal",
    );

    arm_idle_exit(&mut process, &mut output);

    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert!(
        output
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "TUI did not enter the alternate screen"
    );
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "TUI did not restore the primary screen"
    );
}

#[test]
fn tui_permission_round_trips_through_the_runtime_surface() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"suggest\"\n")
        .expect("configure explicit suggest mode");
    const PERMISSION_SENTINEL: &str = "PTY_PERMISSION_RESUMED";
    let prompt = format!(
        "request_permissions_then_bash {} :: printf '\\120\\124\\131\\137\\120\\105\\122\\115\\111\\123\\123\\111\\117\\116\\137\\122\\105\\123\\125\\115\\105\\104'",
        cwd.path().display()
    );
    assert!(
        !prompt.contains(PERMISSION_SENTINEL),
        "the post-permission sentinel must not be present in the rendered prompt"
    );
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), &prompt)
        .expect("spawn permission TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "Filesystem Permission Required",
        Duration::from_secs(10),
        "TUI did not render the runtime-owned permission",
    );
    process.write(b"1").expect("allow permission once");
    receive_until(
        &process,
        &mut output,
        "requested shell",
        Duration::from_secs(10),
        "TUI did not advance to the runtime-owned tool approval",
    );
    process.write(b"1").expect("approve bash once");
    receive_until(
        &process,
        &mut output,
        PERMISSION_SENTINEL,
        Duration::from_secs(10),
        "TUI did not resume after the typed permission response",
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_cancel_returns_to_idle_through_the_runtime_surface() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "mock_stream_delay_ms 10000")
            .expect("spawn cancellable TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "running 0s",
        Duration::from_secs(20),
        "TUI did not render the active turn before cancellation",
    );
    cancel_running_turn_and_exit(&mut process, &mut output);

    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert!(
        !String::from_utf8_lossy(&output).contains("Mock slow stream completed."),
        "cancelled PTY turn must not display a post-terminal completion; output={}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn tui_restart_recovers_history_from_the_runtime_snapshot() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut source = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "pty restart seed")
        .expect("spawn source TUI in PTY");
    let mut source_output = Vec::new();
    receive_until(
        &source,
        &mut source_output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "source TUI did not complete",
    );
    arm_idle_exit(&mut source, &mut source_output);
    let status = source.wait_for_exit(Duration::from_secs(5));
    source.close_io_and_join();
    assert_eq!(status.code(), Some(130), "source TUI exited with {status}");

    let mut resumed =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");
    let mut resumed_output = Vec::new();
    receive_until(
        &resumed,
        &mut resumed_output,
        "Mock history users: pty restart seed | mock_history_echo",
        Duration::from_secs(10),
        "resumed TUI did not hydrate history from the typed snapshot",
    );
    arm_idle_exit(&mut resumed, &mut resumed_output);
    let status = resumed.wait_for_exit(Duration::from_secs(5));
    resumed.close_io_and_join();
    assert_eq!(status.code(), Some(130), "resumed TUI exited with {status}");
}

#[test]
fn tui_side_conversation_is_separate_disposable_and_returns_to_parent() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "main pty seed")
        .expect("spawn parent TUI in PTY");
    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "parent TUI did not complete before Side opened",
    );

    // The slash command opens the empty Side composer. The shortcut resolver is
    // covered separately; this synthetic PTY does not emulate Kitty negotiation.
    process
        .write(b"/side\r")
        .expect("open Side with slash command");
    receive_until(
        &process,
        &mut output,
        "Ctrl+/ to switch",
        Duration::from_secs(5),
        "TUI did not open Side",
    );
    process
        .write(b"mock_history_echo\r")
        .expect("submit Side question");
    receive_until(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        Duration::from_secs(10),
        "Side did not inherit the parent cutover context",
    );

    // Toggling back must restore the parent projection. A parent history echo
    // must not contain the Side-only prompt.
    process
        .write(b"\x1b[47;5u")
        .expect("return to parent with Ctrl+/");
    let parent_toggle_start = output.len();
    receive_until_after(
        &process,
        &mut output,
        "Main · Side available",
        parent_toggle_start,
        Duration::from_secs(5),
        "TUI did not restore the parent while retaining Side",
    );
    let parent_echo_start = output.len();
    process
        .write(b"mock_history_echo\r")
        .expect("submit parent history check");
    receive_until_after(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        parent_echo_start,
        Duration::from_secs(10),
        "parent did not resume after Side toggle",
    );
    assert!(
        !String::from_utf8_lossy(&output[parent_echo_start..])
            .contains("main pty seed | mock_history_echo | mock_history_echo"),
        "Side prompt leaked into the parent transcript"
    );

    // Return to Side and close it. Ctrl+C owns Side cleanup and must not
    // interrupt or close the parent.
    let side_toggle_start = output.len();
    process.write(b"\x1b[47;5u").expect("return to Side");
    receive_until_after(
        &process,
        &mut output,
        "Side from",
        side_toggle_start,
        Duration::from_secs(5),
        "TUI did not reactivate Side",
    );
    process.write(&[0x03]).expect("close Side with Ctrl+C");
    std::thread::sleep(Duration::from_millis(250));
    process.drain_output(&mut output);
    let parent_after_close_start = output.len();
    process
        .write(b"mock_history_echo\r")
        .expect("submit after closing Side");
    receive_until_after(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo | mock_history_echo",
        parent_after_close_start,
        Duration::from_secs(10),
        "closing Side did not restore the parent",
    );
    assert!(
        !String::from_utf8_lossy(&output[parent_after_close_start..])
            .contains("main pty seed | mock_history_echo | mock_history_echo | mock_history_echo"),
        "Side prompt leaked into the parent after close"
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");

    let session_files = count_history_files(&home.path().join("sessions"));
    assert_eq!(session_files, 1, "Side must not create a durable session");
}

// Regression: switching panes must not blank the transcript. The existing
// coverage always submits a fresh prompt after each toggle, which forces a
// re-render and hides whether the toggle itself left the pane empty. This test
// toggles and then inspects the screen WITHOUT submitting anything.
#[test]
fn tui_side_toggle_keeps_transcripts_visible_without_resubmitting() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "main pty seed")
        .expect("spawn parent TUI in PTY");
    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "parent TUI did not complete before Side opened",
    );

    // Open Side and echo the inherited history so the Side transcript carries a
    // marker distinct from the parent.
    process
        .write(b"/side\r")
        .expect("open Side with slash command");
    receive_until(
        &process,
        &mut output,
        "Ctrl+/ to switch",
        Duration::from_secs(5),
        "TUI did not open Side",
    );
    process
        .write(b"mock_history_echo\r")
        .expect("submit Side question");
    receive_until(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        Duration::from_secs(10),
        "Side did not inherit the parent cutover context",
    );

    // Toggle back to the parent. Do NOT submit anything. The parent transcript
    // (its seed prompt) must remain on the reconstructed screen after the switch.
    process
        .write(b"\x1b[47;5u")
        .expect("return to parent with Ctrl+/");
    receive_until(
        &process,
        &mut output,
        "Main · Side available",
        Duration::from_secs(5),
        "TUI did not restore the parent status line",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "> main pty seed",
        "parent transcript went blank after toggling back without resubmitting",
    );

    // Toggle to Side again. Its inherited echo must remain on the reconstructed
    // screen after the switch, again without submitting.
    process.write(b"\x1b[47;5u").expect("return to Side");
    receive_until(
        &process,
        &mut output,
        "Side from",
        Duration::from_secs(5),
        "TUI did not reactivate the Side status line",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        "side transcript went blank after toggling back without resubmitting",
    );

    process.write(&[0x03]).expect("close Side with Ctrl+C");
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

// Reconstructs the on-screen terminal grid from the full PTY byte stream and
// asserts `expected` is visible. Incremental renderers only emit cells that
// change between frames, so a switch that leaves the top rows untouched will
// not re-print them — checking only the post-switch delta would miss content
// that is genuinely on screen. Rebuilding the grid reflects what the user sees.
fn assert_screen_shows(process: &PtyProcess, output: &mut Vec<u8>, expected: &str, failure: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if screen_contains(output, expected) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "{failure}; reconstructed screen=\n{}",
                reconstruct_screen(output)
            );
        }
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn screen_contains(output: &[u8], expected: &str) -> bool {
    let screen = reconstruct_screen(output);
    let mut cursor = 0;
    for token in expected.split_whitespace() {
        let Some(offset) = screen[cursor..].find(token) else {
            return false;
        };
        cursor += offset + token.len();
    }
    true
}

// Minimal ANSI interpreter: honours cursor positioning (CSI row;col H) and the
// erase-screen (CSI 2 J) sequence, dropping other CSI/OSC controls. Enough to
// materialize the visible grid our TUI paints.
fn reconstruct_screen(output: &[u8]) -> String {
    use std::collections::BTreeMap;
    let text = String::from_utf8_lossy(output);
    let mut grid: BTreeMap<(usize, usize), char> = BTreeMap::new();
    let (mut row, mut col) = (1usize, 1usize);
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == '\u{1b}' {
            // OSC: ESC ] ... (BEL | ESC \)
            if bytes.get(i + 1) == Some(&']') {
                i += 2;
                while i < bytes.len() && bytes[i] != '\u{07}' && bytes[i] != '\u{1b}' {
                    i += 1;
                }
                if bytes.get(i) == Some(&'\u{1b}') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            // CSI: ESC [ params final
            if bytes.get(i + 1) == Some(&'[') {
                let mut j = i + 2;
                let mut params = String::new();
                while j < bytes.len() && !bytes[j].is_ascii_alphabetic() {
                    params.push(bytes[j]);
                    j += 1;
                }
                let final_byte = bytes.get(j).copied().unwrap_or(' ');
                match final_byte {
                    'H' | 'f' => {
                        let mut parts = params.split(';');
                        row = parts
                            .next()
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(1)
                            .max(1);
                        col = parts
                            .next()
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(1)
                            .max(1);
                    }
                    'J' => {
                        if params == "2" {
                            grid.clear();
                        }
                    }
                    _ => {}
                }
                i = j + 1;
                continue;
            }
            i += 2;
            continue;
        }
        match ch {
            '\n' => {
                row += 1;
                col = 1;
            }
            '\r' => col = 1,
            _ => {
                grid.insert((row, col), ch);
                col += 1;
            }
        }
        i += 1;
    }
    let max_row = grid.keys().map(|(r, _)| *r).max().unwrap_or(0);
    let mut lines = Vec::new();
    for r in 1..=max_row {
        let cols: Vec<usize> = grid
            .range((r, 0)..(r + 1, 0))
            .map(|((_, c), _)| *c)
            .collect();
        let max_col = cols.iter().copied().max().unwrap_or(0);
        let line: String = (1..=max_col)
            .map(|c| grid.get(&(r, c)).copied().unwrap_or(' '))
            .collect();
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

struct PtyProcess {
    child: Option<Child>,
    writer: Option<File>,
    reader: Option<JoinHandle<()>>,
    output_rx: Receiver<Vec<u8>>,
}

impl PtyProcess {
    fn spawn(home: &std::path::Path, cwd: &std::path::Path) -> io::Result<Self> {
        Self::spawn_with_prompt(home, cwd, PROMPT)
    }

    fn spawn_with_prompt(
        home: &std::path::Path,
        cwd: &std::path::Path,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history(home, cwd, None, prompt)
    }

    fn spawn_resumed(
        home: &std::path::Path,
        cwd: &std::path::Path,
        selector: &str,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history(home, cwd, Some(selector), prompt)
    }

    fn spawn_with_history(
        home: &std::path::Path,
        cwd: &std::path::Path,
        resume: Option<&str>,
        prompt: &str,
    ) -> io::Result<Self> {
        let (master, slave) = open_pty(120, 40)?;
        let stdout = duplicate_fd(&slave)?;
        let stderr = duplicate_fd(&slave)?;
        let writer = File::from(duplicate_fd(&master)?);
        let mut terminal_reader = File::from(master);
        let stdin = File::from(slave);

        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command.args(["--provider", "mock", "--cwd"]).arg(cwd);
        if let Some(selector) = resume {
            command.args(["--resume", selector]);
        }
        let child = command
            .arg(prompt)
            .env("ORCA_HOME", home)
            .env("ORCA_API_KEY", "pty-test-key")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(File::from(stdout)))
            .stderr(Stdio::from(File::from(stderr)))
            .spawn()?;

        let (output_tx, output_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match terminal_reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if output_tx.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("read TUI PTY: {error}"),
                }
            }
        });

        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            reader: Some(reader),
            output_rx,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer is closed"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn receive_output(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.output_rx.recv_timeout(timeout).ok()
    }

    fn drain_output(&self, output: &mut Vec<u8>) {
        output.extend(self.output_rx.try_iter().flatten());
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("PTY child remains owned")
                .try_wait()
                .expect("poll TUI process")
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not exit after idle Ctrl-C"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("PTY child remains owned")
            .try_wait()
    }

    fn close_io_and_join(&mut self) {
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join PTY reader");
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        self.child.take();
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn arm_idle_exit(process: &mut PtyProcess, output: &mut Vec<u8>) {
    std::thread::sleep(Duration::from_millis(250));
    process.drain_output(output);
    process.write(&[0x03]).expect("send first idle Ctrl-C");
    receive_until(
        process,
        output,
        "Press Ctrl+C again to quit.",
        Duration::from_secs(2),
        "TUI did not arm idle exit",
    );
    process.write(&[0x03]).expect("send second idle Ctrl-C");
}

fn cancel_running_turn_and_exit(process: &mut PtyProcess, output: &mut Vec<u8>) {
    const IDLE_EXIT_NOTICE: &str = "Press Ctrl+C again to quit.";

    process.drain_output(output);
    let notice_start = output.len();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if send_ctrl_c_or_observe_idle_exit(process, "interrupt the running turn or arm idle exit")
        {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "TUI did not settle the cancelled turn within 5s; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
        process.drain_output(output);
        if let Some(status) = process.try_wait().expect("poll cancelled TUI exit") {
            assert_eq!(
                status.code(),
                Some(130),
                "cancelled TUI exited with {status}"
            );
            return;
        }
        if contains_rendered_text(&output[notice_start..], IDLE_EXIT_NOTICE) {
            break;
        }
    }
    await_idle_ctrl_c_exit(process);
}

fn send_ctrl_c_or_observe_idle_exit(process: &mut PtyProcess, action: &str) -> bool {
    match process.write(&[0x03]) {
        Ok(()) => false,
        Err(error) => {
            let status = process.wait_for_exit(Duration::from_secs(1));
            assert_eq!(
                status.code(),
                Some(130),
                "{action} failed with {error}; TUI exited with {status}"
            );
            true
        }
    }
}

fn await_idle_ctrl_c_exit(process: &mut PtyProcess) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match process.write(&[0x03]) {
            Ok(()) => {}
            Err(error) => {
                if let Some(status) = process.try_wait().expect("poll idle Ctrl-C exit") {
                    assert_eq!(
                        status.code(),
                        Some(130),
                        "idle Ctrl-C closed the PTY with {error}, but TUI exited with {status}"
                    );
                    return;
                }
            }
        }
        if let Some(status) = process.try_wait().expect("poll idle Ctrl-C exit") {
            assert_eq!(status.code(), Some(130), "TUI exited with {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "TUI did not consume idle Ctrl-C within 2s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn receive_until(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    timeout: Duration,
    failure: &str,
) {
    let deadline = Instant::now() + timeout;
    while !contains_rendered_text(output, expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "{failure}; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn receive_until_after(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    start: usize,
    timeout: Duration,
    failure: &str,
) {
    let deadline = Instant::now() + timeout;
    while !contains_rendered_text(&output[start..], expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "{failure}; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn contains_rendered_text(output: &[u8], expected: &str) -> bool {
    let rendered = String::from_utf8_lossy(output);
    let mut cursor = 0;
    for token in expected.split_whitespace() {
        let Some(offset) = rendered[cursor..].find(token) else {
            return false;
        };
        cursor += offset + token.len();
    }
    true
}

fn count_history_files(root: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_history_files(&path)
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                1
            } else {
                0
            }
        })
        .sum()
}

fn open_pty(columns: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn duplicate_fd(fd: &impl AsRawFd) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}
