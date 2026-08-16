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
    let reset_result = reset_title(&mut terminal);
    drop_terminal(terminal);
    let finish_result = finish_input();
    reset_result.and(finish_result)
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use super::{
        complete_presentation_resume, finish_terminal_presentation,
        initialize_terminal_presentation, with_terminal_presentation_cleanup,
    };

    #[test]
    fn terminal_title_writes_before_initial_draw() {
        let mut calls = Vec::new();
        initialize_terminal_presentation(
            &mut calls,
            |calls| {
                calls.push("write-start");
                Ok(())
            },
            |calls| {
                calls.push("draw-start");
                Ok(())
            },
        )
        .expect("startup presentation");
        assert_eq!(calls, ["write-start", "draw-start"]);
    }

    #[test]
    fn presentation_resume_clears_invalidates_then_marks_dirty() {
        let mut calls = Vec::new();
        complete_presentation_resume(
            &mut calls,
            |calls| {
                calls.push("clear");
                Ok(())
            },
            |calls| calls.push("invalidate"),
            |calls| calls.push("dirty"),
        )
        .expect("resume presentation");
        assert_eq!(calls, ["clear", "invalidate", "dirty"]);

        let mut calls = Vec::new();
        let error = complete_presentation_resume(
            &mut calls,
            |_| Err(io::Error::other("clear failed")),
            |calls| calls.push("invalidate"),
            |calls| calls.push("dirty"),
        )
        .expect_err("clear failure should stop resume");
        assert_eq!(error.to_string(), "clear failed");
        assert!(calls.is_empty());
    }

    #[test]
    fn presentation_exit_resets_drops_then_finishes_input() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let reset_exit = Rc::clone(&calls);
        let drop_exit = Rc::clone(&calls);
        let finish_exit = Rc::clone(&calls);
        finish_terminal_presentation(
            (),
            move |_| {
                reset_exit.borrow_mut().push("reset");
                Ok(())
            },
            move |_| drop_exit.borrow_mut().push("drop"),
            move || {
                finish_exit.borrow_mut().push("finish");
                Ok(())
            },
        )
        .expect("exit presentation");
        assert_eq!(*calls.borrow(), ["reset", "drop", "finish"]);
    }

    #[test]
    fn presentation_exit_cleanup_runs_after_body_error() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let body = Rc::clone(&calls);
        let cleanup = Rc::clone(&calls);

        let error = with_terminal_presentation_cleanup(
            (),
            move |_| {
                body.borrow_mut().push("body");
                Err::<i32, _>(io::Error::other("body failed"))
            },
            move |_| {
                cleanup.borrow_mut().push("cleanup");
                Ok(())
            },
        )
        .expect_err("body error should be preserved");

        assert_eq!(error.to_string(), "body failed");
        assert_eq!(*calls.borrow(), ["body", "cleanup"]);
    }

    #[test]
    fn reset_failure_still_drops_terminal_and_finishes_input() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let reset_calls = Rc::clone(&calls);
        let drop_calls = Rc::clone(&calls);
        let finish_calls = Rc::clone(&calls);

        let error = finish_terminal_presentation(
            (),
            move |_| {
                reset_calls.borrow_mut().push("reset");
                Err(io::Error::other("reset failed"))
            },
            move |_| drop_calls.borrow_mut().push("drop"),
            move || {
                finish_calls.borrow_mut().push("finish");
                Err(io::Error::other("finish failed"))
            },
        )
        .expect_err("reset failure should remain the primary cleanup error");

        assert_eq!(error.to_string(), "reset failed");
        assert_eq!(*calls.borrow(), ["reset", "drop", "finish"]);
    }
}
