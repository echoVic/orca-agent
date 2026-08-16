//! Terminal presentation lifecycle: resume rendering, title write +
//! draw, resume completion, terminal finish, and cleanup scope. Extracted
//! from `app.rs` (TUI convergence slice 2); generic over the terminal
//! target, no AppState coupling.

use std::io;

use crate::frame_scheduler::FrameScheduler;
use crate::terminal_presentation::TerminalPresentation;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::capability_backend::CapabilityBackend;
use crate::stdio_guard::RetryWriter;

pub(crate) type InlineTerminal =
    Terminal<CapabilityBackend<CrosstermBackend<RetryWriter<std::io::Stdout>>>>;

pub(crate) fn resume_terminal_render<B: Backend>(
    terminal: &mut Terminal<B>,
    scheduler: &mut FrameScheduler,
    presentation: &mut TerminalPresentation,
) -> io::Result<()> {
    complete_presentation_resume(
        terminal,
        Terminal::clear,
        |_| presentation.invalidate_title(),
        |_| scheduler.mark_dirty(),
    )
}

pub(crate) fn initialize_terminal_presentation<T>(
    target: &mut T,
    write_title: impl FnOnce(&mut T) -> io::Result<()>,
    draw: impl FnOnce(&mut T) -> io::Result<()>,
) -> io::Result<()> {
    write_title(target)?;
    draw(target)
}

pub(crate) fn complete_presentation_resume<T>(
    target: &mut T,
    clear_terminal: impl FnOnce(&mut T) -> io::Result<()>,
    invalidate_title: impl FnOnce(&mut T),
    mark_dirty: impl FnOnce(&mut T),
) -> io::Result<()> {
    clear_terminal(target)?;
    invalidate_title(target);
    mark_dirty(target);
    Ok(())
}

pub(crate) fn finish_terminal_presentation<T>(
    mut terminal: T,
    reset_title: impl FnOnce(&mut T) -> io::Result<()>,
    drop_terminal: impl FnOnce(T),
    finish_input: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    reset_title(&mut terminal)?;
    drop_terminal(terminal);
    finish_input()
}

pub(crate) fn with_terminal_presentation_cleanup<T, R>(
    mut resource: T,
    body: impl FnOnce(&mut T) -> io::Result<R>,
    cleanup: impl FnOnce(T) -> io::Result<()>,
) -> io::Result<R> {
    let result = body(&mut resource);
    let cleanup_result = cleanup(resource);
    match result {
        Err(error) => Err(error),
        Ok(value) => cleanup_result.map(|()| value),
    }
}
