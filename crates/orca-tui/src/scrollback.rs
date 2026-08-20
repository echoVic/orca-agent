//! Terminal scrollback erasure for the clear-screen shortcut. Extracted
//! from `app.rs` (TUI convergence slice 5).

use std::io;

use ratatui::Terminal;

use crate::presentation::InlineTerminal;

pub(crate) fn clear_terminal_scrollback_with<T>(
    target: &mut T,
    mut move_home: impl FnMut(&mut T) -> io::Result<()>,
    mut clear_all: impl FnMut(&mut T) -> io::Result<()>,
    mut clear_purge: impl FnMut(&mut T) -> io::Result<()>,
    mut clear_frame: impl FnMut(&mut T) -> io::Result<()>,
) -> io::Result<()> {
    move_home(target)?;
    clear_all(target)?;
    clear_purge(target)?;
    clear_frame(target)
}

/// Erase the native scrollback and on-screen content. Used by the clear-screen shortcut so a
/// fresh session starts on a clean terminal instead of stacking under the old transcript.
pub(crate) fn clear_terminal_scrollback(terminal: &mut InlineTerminal) -> io::Result<()> {
    use crossterm::ExecutableCommand;
    use crossterm::terminal::{Clear, ClearType};
    clear_terminal_scrollback_with(
        terminal,
        |terminal| {
            terminal
                .backend_mut()
                .inner_mut()
                .execute(crossterm::cursor::MoveTo(0, 0))?;
            Ok(())
        },
        |terminal| {
            terminal
                .backend_mut()
                .inner_mut()
                .execute(Clear(ClearType::All))?;
            Ok(())
        },
        |terminal| {
            terminal
                .backend_mut()
                .inner_mut()
                .execute(Clear(ClearType::Purge))?;
            Ok(())
        },
        Terminal::clear,
    )
}
