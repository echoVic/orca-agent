//! SIGINT, SIGTERM and SIGHUP in the TUI (unix): each quits it as an
//! ordinary exit does, with the signal's exit code, 128 plus its number.
//!
//! Left to their default disposition they end the process at once, leaving
//! the terminal in raw mode on the alternate screen, the stdio MCP servers
//! running, and the session unsaved. The shared core of
//! [`orca_runtime::termination_signals`] takes them over and bounds the quit
//! that follows; the renderer quits on [`TuiEvent::TerminationSignal`]
//! through `finish_tui_run`, as it does on its own. A terminal that hung up
//! is reported the same way as SIGHUP, which usually comes with it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use orca_runtime::termination_signals::{
    TerminationSignal, TerminationSignalGuard, handle_termination_signals,
};

use crate::channels::TuiEventSender;
use crate::input_runtime::TerminationTakeover;
use crate::protocol::TuiEvent;

/// How long input waits to report a hangup while the renderer's mailbox is
/// full. The renderer drains it every frame while it runs; once it has quit,
/// the report is moot, and waiting longer would hold up its teardown.
const HANGUP_REPORT_TIMEOUT: Duration = Duration::from_secs(1);

/// Hands SIGINT, SIGTERM and SIGHUP to the renderer as
/// [`TuiEvent::TerminationSignal`], and returns once that is in effect.
///
/// The core bounds the quit that follows: if `finished` is still unset when
/// its grace period ends, or at a second signal, the process exits without
/// waiting, once [`put_the_terminal_back`] has had its chance. Set
/// `finished` once the TUI has cleaned up, and hold the guard until the
/// process exits. `None` when the handlers could not be set up: the signals
/// then end the TUI as they always did.
pub(crate) fn install_tui_termination_signals(
    event_tx: TuiEventSender,
    finished: Arc<AtomicBool>,
) -> Option<TerminationSignalGuard> {
    handle_termination_signals(
        &[
            TerminationSignal::Interrupt,
            TerminationSignal::Terminate,
            TerminationSignal::Hangup,
        ],
        finished,
        // On a thread of its own: waiting for room in a full mailbox holds
        // up nothing that bounds the quit.
        move |signal| {
            let _ = event_tx.send(TuiEvent::TerminationSignal { signal });
        },
        put_the_terminal_back,
    )
}

/// What the terminal session takes from the TUI's handling of the end of a
/// session, once [`install_tui_termination_signals`] is in effect: SIGINT and
/// SIGTERM are left to it, and a terminal that hung up (its input ended or
/// failed) is reported as the SIGHUP that usually comes with it, so that
/// whichever the renderer sees first, it quits the same way.
pub(crate) fn hangup_takeover(event_tx: TuiEventSender) -> TerminationTakeover {
    TerminationTakeover {
        report_hangup: Box::new(move || {
            let _ = event_tx.send_timeout(
                TuiEvent::TerminationSignal {
                    signal: TerminationSignal::Hangup,
                },
                HANGUP_REPORT_TIMEOUT,
            );
        }),
    }
}

/// Puts the terminal back as the TUI's teardown does, for an exit that
/// cannot wait for it: shows the cursor ratatui hid, then leaves what the
/// terminal session entered (raw mode, the alternate screen, mouse
/// reporting, bracketed paste, focus events, keyboard enhancement).
///
/// The renderer may be anywhere, a frame half written: this takes no lock
/// it may hold (`io::stdout()` would), writes to the descriptors directly,
/// and ignores every failure. After a SIGHUP the writes simply fail.
fn put_the_terminal_back() {
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    let _ = unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            SHOW_CURSOR.as_ptr().cast(),
            SHOW_CURSOR.len(),
        )
    };
    crate::input_runtime::restore_terminal_now();
}
