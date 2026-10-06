//! Signals that ask the process to stop, taken over instead of ending it.
//!
//! Left to their default disposition, SIGINT, SIGTERM and SIGHUP end the
//! process at once: the child processes it started, task commands and stdio
//! MCP servers, outlive it, and what it was recording ends without a terminal
//! record. [`handle_termination_signals`] takes them over on a thread of its
//! own. The caller says what the first one does (`orca exec` interrupts its
//! run, the TUI quits as it does on its own), and the thread bounds the
//! cleanup that follows: once [`INTERRUPT_GRACE_PERIOD`] passes before the
//! caller reports it finished, or at a second signal, the process exits with
//! the signal's conventional code, 128 plus its number.

use std::future::poll_fn;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

/// How long a signal-interrupted run may spend stopping task-owned commands
/// and committing its terminal record before it is killed outright.
pub const INTERRUPT_GRACE_PERIOD: Duration = Duration::from_secs(10);
/// Poll interval used while waiting for an interrupted run to finish cleanup.
const INTERRUPT_GRACE_POLL: Duration = Duration::from_millis(25);

/// A signal that asks the process to stop.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TerminationSignal {
    /// SIGINT: Ctrl+C in a terminal or a wrapper script, or `kill -INT`. On
    /// Windows, the console's Ctrl+C, the only one of these it delivers.
    Interrupt,
    /// SIGTERM: `kill`, a CI cancel or a job timeout.
    Terminate,
    /// SIGHUP: the controlling terminal went away.
    Hangup,
}

impl TerminationSignal {
    /// The signal's number, the same on every unix.
    pub fn number(self) -> i32 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }

    /// The exit code that reports the signal: 128 plus its number.
    pub fn exit_code(self) -> i32 {
        128 + self.number()
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

/// Keeps the thread of [`handle_termination_signals`] waiting for a first
/// signal. Dropping it before one came ends the thread; once one came, the
/// thread sees its grace period through either way. Tokio never gives a
/// signal back its default disposition, so after the thread ends these
/// signals do nothing: hold it until the process is about to exit.
#[must_use = "dropping it stops handling the signals"]
pub struct TerminationSignalGuard {
    _stop: tokio::sync::oneshot::Sender<()>,
}

/// Handles `signals` on a thread of its own, named `orca-signal`, and returns
/// once they are in effect: from then on none of them ends the process
/// outright.
///
/// The first one calls `on_first_signal` on that thread and starts the grace
/// period. If `finished` is still unset when [`INTERRUPT_GRACE_PERIOD`] has
/// passed, the process exits with the signal's exit code; a second signal,
/// of any of them, exits at once with its own. Each exit says why on stderr.
/// Once `finished` is set, a signal is ignored and the thread ends.
///
/// Windows handles only [`TerminationSignal::Interrupt`], as the console's
/// Ctrl+C. Returns `None`, handling none of them, when the thread or its
/// handlers could not be set up.
#[must_use = "dropping the guard stops handling the signals"]
pub fn handle_termination_signals(
    signals: &[TerminationSignal],
    finished: Arc<AtomicBool>,
    on_first_signal: impl FnOnce(TerminationSignal) + Send + 'static,
) -> Option<TerminationSignalGuard> {
    let signals = signals.to_vec();
    let (registered_tx, registered_rx) = std::sync::mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("orca-signal".to_string())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let Ok(mut listeners) = Listeners::register(&signals) else {
                    return;
                };
                let _ = registered_tx.send(());
                let first = tokio::select! {
                    signal = listeners.next() => signal,
                    _ = stop_rx => return,
                };
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                // Whatever the caller's action does, the grace period and the
                // second signal still bound the cleanup.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    on_first_signal(first)
                }));
                bound_the_cleanup(&mut listeners, first, &finished).await;
            });
        })
        .ok()?;
    registered_rx.recv().ok()?;
    Some(TerminationSignalGuard { _stop: stop_tx })
}

/// Sees the grace period of `first` through: returns once `finished` is set,
/// and ends the process when the period passes first, or at a second signal
/// while the cleanup still runs. A second signal always exits immediately:
/// the grace period exists to let cleanup stop task-owned commands, not to
/// trap an impatient operator.
async fn bound_the_cleanup(
    listeners: &mut Listeners,
    first: TerminationSignal,
    finished: &AtomicBool,
) {
    let grace = tokio::time::sleep(INTERRUPT_GRACE_PERIOD);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            () = &mut grace => {
                if !finished.load(Ordering::SeqCst) {
                    say(format_args!(
                        "orca: cleanup did not finish within {}s; exiting",
                        INTERRUPT_GRACE_PERIOD.as_secs()
                    ));
                    std::process::exit(first.exit_code());
                }
                return;
            }
            second = listeners.next() => {
                // Finished meanwhile: nothing is left to cut short.
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                say(format_args!(
                    "orca: received a second {}; exiting immediately",
                    second.name()
                ));
                std::process::exit(second.exit_code());
            }
            () = tokio::time::sleep(INTERRUPT_GRACE_POLL) => {
                if finished.load(Ordering::SeqCst) {
                    return;
                }
            }
        }
    }
}

/// Says `message` on stderr, dropping it if the write fails: after a SIGHUP
/// the terminal can be gone, and a panic here would end this thread instead
/// of the process.
fn say(message: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "{message}");
}

/// The handlers registered for the signals a thread waits for.
struct Listeners {
    #[cfg(unix)]
    unix: Vec<(TerminationSignal, tokio::signal::unix::Signal)>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
}

impl Listeners {
    /// Registers a handler for each of `signals`, failing if any one cannot
    /// be. Called inside the runtime, whose driver delivers them.
    fn register(signals: &[TerminationSignal]) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let unix = signals
                .iter()
                .map(|&termination| {
                    let kind = match termination {
                        TerminationSignal::Interrupt => SignalKind::interrupt(),
                        TerminationSignal::Terminate => SignalKind::terminate(),
                        TerminationSignal::Hangup => SignalKind::hangup(),
                    };
                    Ok((termination, signal(kind)?))
                })
                .collect::<io::Result<Vec<_>>>()?;
            Ok(Self { unix })
        }
        #[cfg(windows)]
        {
            let ctrl_c = if signals.contains(&TerminationSignal::Interrupt) {
                Some(tokio::signal::windows::ctrl_c()?)
            } else {
                None
            };
            Ok(Self { ctrl_c })
        }
    }

    /// The next signal any of them receives.
    async fn next(&mut self) -> TerminationSignal {
        poll_fn(|context| self.poll_next(context)).await
    }

    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<TerminationSignal> {
        // A handler yields `None` only once its runtime shuts down, and then
        // never fires again: it is left pending.
        #[cfg(unix)]
        for (termination, listener) in &mut self.unix {
            if let Poll::Ready(Some(())) = listener.poll_recv(context) {
                return Poll::Ready(*termination);
            }
        }
        #[cfg(windows)]
        if let Some(ctrl_c) = &mut self.ctrl_c
            && let Poll::Ready(Some(())) = ctrl_c.poll_recv(context)
        {
            return Poll::Ready(TerminationSignal::Interrupt);
        }
        Poll::Pending
    }
}
