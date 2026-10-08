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

/// Clears the whole screen and makes the next draw repaint every cell, which
/// is what `Terminal::clear` did for a fullscreen terminal before ratatui 0.30.
///
/// Since 0.30 `Terminal::clear` first asks the terminal where the cursor is,
/// so that it can put the cursor back. The answer comes in on the terminal
/// input, which the TUI reads itself, so crossterm never sees it and the call
/// fails after a two-second wait. A resize to the current size clears the
/// same way without asking, and the next draw places the cursor.
pub(crate) fn clear_terminal<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let area = terminal.size()?.into();
    terminal.resize(area)
}

pub(crate) fn resume_terminal_render<B: Backend>(
    terminal: &mut Terminal<B>,
    scheduler: &mut FrameScheduler,
    presentation: &mut TerminalPresentation,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    complete_presentation_resume(
        terminal,
        |terminal| clear_terminal(terminal).map_err(io::Error::other),
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
    use std::convert::Infallible;
    use std::io;
    use std::ops::Range;
    use std::rc::Rc;

    use ratatui::Terminal;
    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};
    use ratatui::widgets::Paragraph;

    use super::{
        clear_terminal, complete_presentation_resume, finish_terminal_presentation,
        initialize_terminal_presentation, with_terminal_presentation_cleanup,
    };

    /// A terminal whose cursor position cannot be read, like the TUI's own:
    /// the answer to crossterm's query arrives on the input the TUI reads.
    struct CursorBlindBackend(TestBackend);

    fn infallible<T>(result: Result<T, Infallible>) -> io::Result<T> {
        result.map_err(|never| match never {})
    }

    impl Backend for CursorBlindBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            infallible(self.0.draw(content))
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            infallible(self.0.hide_cursor())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            infallible(self.0.show_cursor())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Err(io::Error::other(
                "The cursor position could not be read within a normal duration",
            ))
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            infallible(self.0.set_cursor_position(position))
        }

        fn clear(&mut self) -> io::Result<()> {
            infallible(self.0.clear())
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            infallible(self.0.clear_region(clear_type))
        }

        fn size(&self) -> io::Result<Size> {
            infallible(self.0.size())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            infallible(self.0.window_size())
        }

        fn flush(&mut self) -> io::Result<()> {
            infallible(self.0.flush())
        }

        fn scroll_region_up(&mut self, region: Range<u16>, line_count: u16) -> io::Result<()> {
            infallible(self.0.scroll_region_up(region, line_count))
        }

        fn scroll_region_down(&mut self, region: Range<u16>, line_count: u16) -> io::Result<()> {
            infallible(self.0.scroll_region_down(region, line_count))
        }
    }

    #[test]
    fn clearing_the_terminal_never_asks_where_the_cursor_is() {
        let draw = |terminal: &mut Terminal<CursorBlindBackend>| {
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new("orca"), frame.area()))
                .map(|_| ())
                .unwrap();
        };
        let mut terminal = Terminal::new(CursorBlindBackend(TestBackend::new(6, 2))).unwrap();
        draw(&mut terminal);

        clear_terminal(&mut terminal).expect("clearing must not need the cursor position");
        terminal
            .backend()
            .0
            .assert_buffer_lines(["      ", "      "]);

        // The same frame again is painted in full, not skipped as unchanged.
        draw(&mut terminal);
        terminal
            .backend()
            .0
            .assert_buffer_lines(["orca  ", "      "]);
    }

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
