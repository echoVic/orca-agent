use std::io;
use std::io::Write;

use crossterm::ExecutableCommand;
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use orca_core::config::RunConfig;
use orca_runtime::update_check::{
    UpdateAction, UpdateInfo, UpdatePreflight, UpdateRunOutcome, current_update_action,
    dismiss_version, run_update, update_preflight,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdatePromptChoice {
    UpdateNow,
    Skip,
    SkipUntilNext,
    Quit,
}

impl UpdatePromptChoice {
    fn next(self) -> Self {
        match self {
            Self::UpdateNow => Self::Skip,
            Self::Skip => Self::SkipUntilNext,
            Self::SkipUntilNext | Self::Quit => Self::UpdateNow,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::UpdateNow | Self::Quit => Self::SkipUntilNext,
            Self::Skip => Self::UpdateNow,
            Self::SkipUntilNext => Self::Skip,
        }
    }
}

pub fn run(config: RunConfig) -> i32 {
    // The npm wrapper (a Node process) may have flipped O_NONBLOCK on the tty
    // fds we inherit; clearing it before any terminal I/O keeps reads/writes
    // blocking so a resize redraw storm can't fail with EAGAIN (os error 35).
    crate::stdio_guard::clear_stdio_nonblocking();

    match update_preflight(config.update_check, &config.app_version) {
        UpdatePreflight::Continue => {}
        UpdatePreflight::Prompt(info) => {
            #[cfg(unix)]
            let _signals = quit_before_the_tui_on_termination_signals();
            match prompt_for_update(&info) {
                Ok(UpdatePromptChoice::UpdateNow) => {
                    return run_upgrade_command(&current_update_action());
                }
                Ok(UpdatePromptChoice::Skip) => {}
                Ok(UpdatePromptChoice::SkipUntilNext) => {
                    if let Err(error) = dismiss_version(&info.latest) {
                        eprintln!("orca: warning: failed to save update dismissal: {error}");
                    }
                }
                Ok(UpdatePromptChoice::Quit) => return 130,
                Err(error) => {
                    eprintln!("orca: warning: failed to read update choice: {error}");
                }
            }
        }
    }

    crate::app::run_tui(config)
}

/// Attach the normal renderer to a daemon-owned ACP session. No runtime host,
/// provider credentials, or session writer is created in this process.
pub fn run_attached(config: RunConfig, socket: std::path::PathBuf, session: String) -> i32 {
    #[cfg(not(unix))]
    {
        let _ = (config, socket, session);
        eprintln!("orca: local ACP attachment requires Unix; standalone TUI remains available");
        1
    }
    #[cfg(unix)]
    {
        crate::stdio_guard::clear_stdio_nonblocking();
        crate::app::run_tui_attached(config, crate::acp_client::AttachOptions { socket, session })
    }
}

fn prompt_for_update(info: &UpdateInfo) -> io::Result<UpdatePromptChoice> {
    let mut stdout = io::stdout();
    let mut highlighted = UpdatePromptChoice::UpdateNow;
    let action = current_update_action();

    terminal::enable_raw_mode()?;
    let raw_mode = RawModeGuard;
    render_update_prompt(&mut stdout, info, highlighted, &action)?;

    let choice = loop {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
            {
                break UpdatePromptChoice::Quit;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => highlighted = highlighted.prev(),
                KeyCode::Down | KeyCode::Char('j') => highlighted = highlighted.next(),
                KeyCode::Char('1') => break UpdatePromptChoice::UpdateNow,
                KeyCode::Char('2') => break UpdatePromptChoice::Skip,
                KeyCode::Char('3') => break UpdatePromptChoice::SkipUntilNext,
                KeyCode::Enter => break highlighted,
                KeyCode::Esc => break UpdatePromptChoice::Skip,
                _ => {}
            }
            render_update_prompt(&mut stdout, info, highlighted, &action)?;
        }
    };

    drop(raw_mode);
    stdout.execute(cursor::MoveToColumn(0))?;
    writeln!(stdout)?;
    Ok(choice)
}

fn render_update_prompt(
    stdout: &mut io::Stdout,
    info: &UpdateInfo,
    highlighted: UpdatePromptChoice,
    action: &UpdateAction,
) -> io::Result<()> {
    stdout.execute(cursor::MoveToColumn(0))?;
    stdout.execute(terminal::Clear(terminal::ClearType::FromCursorDown))?;
    write_update_prompt_body(stdout, info, highlighted, &action.command_display())
}

fn write_update_prompt_body(
    writer: &mut impl Write,
    info: &UpdateInfo,
    highlighted: UpdatePromptChoice,
    command_display: &str,
) -> io::Result<()> {
    write!(
        writer,
        "Update available! {} -> {}\r\n",
        info.current, info.latest
    )?;
    write!(writer, "Release notes: {}\r\n", info.url)?;
    write!(writer, "\r\n")?;
    write_update_choice_row(
        writer,
        1,
        "Update now",
        Some(command_display),
        highlighted == UpdatePromptChoice::UpdateNow,
    )?;
    write_update_choice_row(
        writer,
        2,
        "Skip",
        None,
        highlighted == UpdatePromptChoice::Skip,
    )?;
    write_update_choice_row(
        writer,
        3,
        "Skip until next version",
        None,
        highlighted == UpdatePromptChoice::SkipUntilNext,
    )?;
    write!(writer, "\r\n")?;
    write!(writer, "Use Up/Down or j/k, then Enter")?;
    writer.flush()
}

fn write_update_choice_row(
    writer: &mut impl Write,
    number: usize,
    label: &str,
    detail: Option<&str>,
    selected: bool,
) -> io::Result<()> {
    let marker = if selected { ">" } else { " " };
    write!(writer, "{marker} {number}. {label}")?;
    if let Some(detail) = detail {
        write!(writer, " (runs \u{60}{detail}\u{60})")?;
    }
    write!(writer, "\r\n")
}

fn run_upgrade_command(action: &UpdateAction) -> i32 {
    println!(
        "Updating Orca via \u{60}{}\u{60}...",
        action.command_display()
    );
    match run_update(action) {
        UpdateRunOutcome::Updated => {
            println!("Upgrade successful. Please restart orca.");
            0
        }
        UpdateRunOutcome::Started => {
            println!("Upgrade started. Orca will be replaced after this process exits.");
            0
        }
        UpdateRunOutcome::Failed(code) => {
            eprintln!(
                "orca: upgrade failed{}",
                code.map(|code| format!(" with exit code {code}"))
                    .unwrap_or_default()
            );
            1
        }
        UpdateRunOutcome::StartFailed(error) => {
            eprintln!("orca: failed to start upgrade command: {error}");
            1
        }
    }
}

/// SIGINT, SIGTERM and SIGHUP while the update prompt waits, or an update
/// it started runs, end orca at once with the signal's exit code, as Ctrl+C
/// does there, once the terminal has left raw mode: nothing else has started
/// yet. Left to their default disposition they ended it in raw mode. Hold
/// the guard until the TUI takes the signals over. `None` when the handlers
/// could not be set up.
#[cfg(unix)]
fn quit_before_the_tui_on_termination_signals()
-> Option<orca_runtime::termination_signals::TerminationSignalGuard> {
    use orca_runtime::termination_signals::{TerminationSignal, handle_termination_signals};
    handle_termination_signals(
        &[
            TerminationSignal::Interrupt,
            TerminationSignal::Terminate,
            TerminationSignal::Hangup,
        ],
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |signal| {
            leave_raw_mode_now();
            std::process::exit(signal.exit_code());
        },
        leave_raw_mode_now,
    )
}

/// Leaves raw mode and the prompt's line, whatever the prompt is doing.
#[cfg(unix)]
fn leave_raw_mode_now() {
    let _ = terminal::disable_raw_mode();
    // Straight to the descriptor: the prompt may hold stdout's lock.
    let _ = unsafe { libc::write(libc::STDOUT_FILENO, b"\r\n".as_ptr().cast(), 2) };
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use orca_runtime::update_check::{UpdateAction, UpdateInfo};

    use super::*;

    /// SIGTERM while the update prompt waits ends orca with SIGTERM's exit
    /// code, after leaving raw mode, instead of killing it in raw mode.
    #[cfg(unix)]
    #[test]
    fn a_termination_signal_before_the_tui_exits_with_its_code() {
        const CHILD_ENV: &str = "ORCA_TEST_SIGNAL_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let _signals = quit_before_the_tui_on_termination_signals();
            assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0, "raise SIGTERM");
            std::thread::sleep(std::time::Duration::from_secs(5));
            return;
        }
        let started = std::time::Instant::now();
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "cli::tests::a_termination_signal_before_the_tui_exits_with_its_code",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("run the test in a process of its own");
        assert_eq!(
            child.status.code(),
            Some(143),
            "{}; stderr: {}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "the signal must end it at once"
        );
    }

    #[test]
    fn update_prompt_choice_navigation_wraps() {
        assert_eq!(
            UpdatePromptChoice::UpdateNow.next(),
            UpdatePromptChoice::Skip
        );
        assert_eq!(
            UpdatePromptChoice::Skip.next(),
            UpdatePromptChoice::SkipUntilNext
        );
        assert_eq!(
            UpdatePromptChoice::SkipUntilNext.next(),
            UpdatePromptChoice::UpdateNow
        );
        assert_eq!(
            UpdatePromptChoice::UpdateNow.prev(),
            UpdatePromptChoice::SkipUntilNext
        );
    }

    #[test]
    fn update_prompt_body_lists_all_choices_and_runtime_command() {
        let mut output = Vec::new();
        let info = UpdateInfo {
            current: "0.1.7".to_string(),
            latest: "0.1.8".to_string(),
            url: "https://example.test/releases/tag/v0.1.8".to_string(),
        };
        let action = UpdateAction::NpmGlobalLatest;

        write_update_prompt_body(
            &mut output,
            &info,
            UpdatePromptChoice::UpdateNow,
            &action.command_display(),
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Update available! 0.1.7 -> 0.1.8\r\n"));
        assert!(output.contains("> 1. Update now (runs `npm install -g"));
        assert!(output.contains("  2. Skip\r\n"));
        assert!(output.contains("  3. Skip until next version\r\n"));
    }
}
