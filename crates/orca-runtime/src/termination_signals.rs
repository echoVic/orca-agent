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
//! the signal's conventional code, 128 plus its number, after a bounded last
//! chance for the caller to put back what must not outlive it (the TUI's
//! terminal modes). A copy of the first signal that comes with it, as a
//! closing terminal or a wrapper script delivers, is not a second one.

use std::future::poll_fn;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// How long a signal-interrupted run may spend stopping task-owned commands
/// and committing its terminal record before it is killed outright.
pub const INTERRUPT_GRACE_PERIOD: Duration = Duration::from_secs(10);
/// Poll interval used while waiting for an interrupted run to finish cleanup.
const INTERRUPT_GRACE_POLL: Duration = Duration::from_millis(25);
/// How long a forced exit waits for the caller's `before_exit`.
pub const BEFORE_EXIT_BOUND: Duration = Duration::from_millis(500);
/// How long after the first signal another one is taken for a copy of the
/// same delivery, and ignored: a terminal that closes, or a wrapper script
/// that passes on what its process group got, delivers a signal twice, a
/// millisecond or so apart.
pub const DUPLICATE_DELIVERY_WINDOW: Duration = Duration::from_millis(500);

/// A signal that asks the process to stop.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TerminationSignal {
    /// SIGINT: Ctrl+C in a terminal or a wrapper script, or `kill -INT`. On
    /// Windows, the console's Ctrl+C.
    Interrupt,
    /// SIGTERM: `kill`, a CI cancel or a job timeout. On Windows, the
    /// console's Ctrl+Break, which a job runner sends to a process group to
    /// stop it, and the user logging off or the system shutting down.
    Terminate,
    /// SIGHUP: the controlling terminal went away. On Windows, its console
    /// window closed.
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
/// The first one calls `on_first_signal` on another thread of its own and
/// starts the grace period at once, so an action that blocks (on a reply that
/// never comes, say) delays neither of the exits that bound it: if `finished`
/// is still unset when [`INTERRUPT_GRACE_PERIOD`] has passed, the process
/// exits with the signal's exit code, and a second signal, of any of them,
/// exits at once with its own. A signal within [`DUPLICATE_DELIVERY_WINDOW`]
/// of the first is not a second one but a copy of the same delivery, and is
/// ignored: a terminal that closes, or a wrapper script that passes on what
/// its process group got, delivers a signal twice, a millisecond or so apart.
/// A forced exit first calls `before_exit`, once, again on a thread of its
/// own, and waits for it [`BEFORE_EXIT_BOUND`] at most; then it says why on
/// stderr. Once `finished` is set, a signal is ignored, the thread ends, and
/// `before_exit` never runs.
///
/// On Windows the console's events stand for them (see
/// [`TerminationSignal`]). Windows ends the process a few seconds after it
/// reports a closed console, a logoff or a shutdown, whatever the grace
/// period. Returns `None`, handling none of them, when the thread or its
/// handlers could not be set up.
#[must_use = "dropping the guard stops handling the signals"]
pub fn handle_termination_signals(
    signals: &[TerminationSignal],
    finished: Arc<AtomicBool>,
    on_first_signal: impl FnOnce(TerminationSignal) + Send + 'static,
    before_exit: impl FnOnce() + Send + 'static,
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
                let first_arrived = Instant::now();
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                // Should its thread not start, the action does not run: the
                // grace period still ends the process.
                let _ = std::thread::Builder::new()
                    .name("orca-signal-action".to_string())
                    .spawn(move || on_first_signal(first));
                bound_the_cleanup(&mut listeners, first, first_arrived, &finished, before_exit)
                    .await;
            });
        })
        .ok()?;
    registered_rx.recv().ok()?;
    Some(TerminationSignalGuard { _stop: stop_tx })
}

/// Sees the grace period of `first`, which arrived at `first_arrived`,
/// through: returns once `finished` is set, and ends the process when the
/// period passes first, or at a second signal while the cleanup still runs.
/// A second signal always exits immediately: the grace period exists to let
/// cleanup stop task-owned commands, not to trap an impatient operator. One
/// within [`DUPLICATE_DELIVERY_WINDOW`] of the first, though, is a copy of
/// it, which no operator sent, and cutting the cleanup short for it would
/// leave behind what the first one asked to stop.
async fn bound_the_cleanup(
    listeners: &mut Listeners,
    first: TerminationSignal,
    first_arrived: Instant,
    finished: &AtomicBool,
    before_exit: impl FnOnce() + Send + 'static,
) {
    let grace = tokio::time::sleep(INTERRUPT_GRACE_PERIOD);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            () = &mut grace => {
                if !finished.load(Ordering::SeqCst) {
                    exit_with(first.exit_code(), before_exit, format_args!(
                        "orca: cleanup did not finish within {}s; exiting",
                        INTERRUPT_GRACE_PERIOD.as_secs()
                    ));
                }
                return;
            }
            second = listeners.next() => {
                // Finished meanwhile: nothing is left to cut short.
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                if first_arrived.elapsed() >= DUPLICATE_DELIVERY_WINDOW {
                    exit_with(second.exit_code(), before_exit, format_args!(
                        "orca: received a second {}; exiting immediately",
                        second.name()
                    ));
                }
            }
            () = tokio::time::sleep(INTERRUPT_GRACE_POLL) => {
                if finished.load(Ordering::SeqCst) {
                    return;
                }
            }
        }
    }
}

/// Ends the process with `code` and says `why` on stderr, once `before_exit`
/// has run, on a thread of its own, or [`BEFORE_EXIT_BOUND`] has passed: a
/// `before_exit` that blocks, on a terminal write or a lock another thread
/// holds, cannot keep the process alive. Should its thread not start, it does
/// not run.
fn exit_with(
    code: i32,
    before_exit: impl FnOnce() + Send + 'static,
    why: std::fmt::Arguments<'_>,
) -> ! {
    let (done_tx, done) = std::sync::mpsc::channel::<()>();
    let started = std::thread::Builder::new()
        .name("orca-signal-exit".to_string())
        .spawn(move || {
            before_exit();
            let _ = done_tx.send(());
        });
    if started.is_ok() {
        // Returns early, too, when `before_exit` panics.
        let _ = done.recv_timeout(BEFORE_EXIT_BOUND);
    }
    say(why);
    std::process::exit(code);
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
    windows: Vec<(TerminationSignal, ConsoleEvent)>,
}

/// A handler for one of the console's events, which stand for the signals on
/// Windows.
#[cfg(windows)]
enum ConsoleEvent {
    CtrlC(tokio::signal::windows::CtrlC),
    CtrlBreak(tokio::signal::windows::CtrlBreak),
    CtrlClose(tokio::signal::windows::CtrlClose),
    CtrlLogoff(tokio::signal::windows::CtrlLogoff),
    CtrlShutdown(tokio::signal::windows::CtrlShutdown),
}

#[cfg(windows)]
impl ConsoleEvent {
    /// The handlers of the events that stand for `termination`.
    fn register(termination: TerminationSignal) -> io::Result<Vec<Self>> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        Ok(match termination {
            TerminationSignal::Interrupt => vec![Self::CtrlC(ctrl_c()?)],
            TerminationSignal::Terminate => vec![
                Self::CtrlBreak(ctrl_break()?),
                Self::CtrlLogoff(ctrl_logoff()?),
                Self::CtrlShutdown(ctrl_shutdown()?),
            ],
            TerminationSignal::Hangup => vec![Self::CtrlClose(ctrl_close()?)],
        })
    }

    fn poll_recv(&mut self, context: &mut Context<'_>) -> Poll<Option<()>> {
        match self {
            Self::CtrlC(listener) => listener.poll_recv(context),
            Self::CtrlBreak(listener) => listener.poll_recv(context),
            Self::CtrlClose(listener) => listener.poll_recv(context),
            Self::CtrlLogoff(listener) => listener.poll_recv(context),
            Self::CtrlShutdown(listener) => listener.poll_recv(context),
        }
    }
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
            let mut windows = Vec::new();
            for &termination in signals {
                for event in ConsoleEvent::register(termination)? {
                    windows.push((termination, event));
                }
            }
            Ok(Self { windows })
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
        for (termination, listener) in &mut self.windows {
            if let Poll::Ready(Some(())) = listener.poll_recv(context) {
                return Poll::Ready(*termination);
            }
        }
        Poll::Pending
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write as _;
    use std::process::Output;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        BEFORE_EXIT_BOUND, DUPLICATE_DELIVERY_WINDOW, TerminationSignal, handle_termination_signals,
    };

    /// How long a child waits for the core to end it after the signal that
    /// should: well under the grace period, which an exit by then is not.
    const EXIT_DEADLINE: Duration = Duration::from_millis(2500);
    /// The child's exit code when the core did not end it by EXIT_DEADLINE.
    const NOT_ENDED: i32 = 99;

    /// Each test raises signals that end its process, so it runs in a
    /// process of its own: the test binary run again for just that test,
    /// with an Orca home of its own. Returns that process's output, or
    /// `None` in that process, where the test goes on.
    fn in_a_process_of_its_own(test: &str) -> Option<Output> {
        const CHILD_ENV: &str = "ORCA_TEST_SIGNAL_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            return None;
        }
        let home = tempfile::tempdir().expect("an Orca home for the child");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", &format!("termination_signals::tests::{test}")])
            .args(["--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env("ORCA_HOME", home.path())
            .output()
            .expect("run the test in a process of its own");
        Some(output)
    }

    fn raise(signal: libc::c_int) {
        assert_eq!(unsafe { libc::raise(signal) }, 0, "raise signal {signal}");
    }

    /// Waits for the core to end this process, which it does before
    /// EXIT_DEADLINE when it works, and otherwise exits with NOT_ENDED.
    fn wait_to_be_ended() -> ! {
        std::thread::sleep(EXIT_DEADLINE);
        std::process::exit(NOT_ENDED);
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Waits out the window in which another signal is a copy of the first,
    /// so that the next one is a second signal.
    fn wait_out_the_copies() {
        std::thread::sleep(DUPLICATE_DELIVERY_WINDOW + Duration::from_millis(100));
    }

    /// The time on the monotonic clock, which every process on the system
    /// reads alike.
    fn monotonic_now() -> Duration {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) },
            0,
            "read the monotonic clock"
        );
        Duration::new(
            u64::try_from(now.tv_sec).expect("seconds since boot"),
            u32::try_from(now.tv_nsec).expect("nanoseconds"),
        )
    }

    /// What a child says on stderr just before it raises the signal that
    /// forces its exit, with the time on the monotonic clock.
    const FORCING_AT: &str = "raising the second signal at ns ";

    /// A copy of a signal comes with it: a terminal that closes, or a
    /// wrapper script that passes on what its process group got, delivers
    /// it twice, a millisecond or so apart. The copy, of that signal or of
    /// another one handled, is the same delivery: it cuts nothing short.
    #[test]
    fn a_copy_that_comes_with_the_first_signal_is_the_same_delivery() {
        let Some(child) =
            in_a_process_of_its_own("a_copy_that_comes_with_the_first_signal_is_the_same_delivery")
        else {
            let finished = Arc::new(AtomicBool::new(false));
            let (acting_tx, acting) = mpsc::channel();
            let _signals = handle_termination_signals(
                &[TerminationSignal::Interrupt, TerminationSignal::Terminate],
                Arc::clone(&finished),
                move |signal| {
                    let _ = acting_tx.send(signal);
                },
                || {},
            )
            .expect("the signal handlers");
            raise(libc::SIGTERM);
            assert_eq!(
                acting
                    .recv_timeout(Duration::from_secs(1))
                    .expect("the first signal's action ran"),
                TerminationSignal::Terminate
            );
            std::thread::sleep(Duration::from_millis(1));
            raise(libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(1));
            raise(libc::SIGINT);
            // A second signal would have ended the process by now; the
            // cleanup goes on and finishes.
            std::thread::sleep(Duration::from_millis(200));
            finished.store(true, Ordering::SeqCst);
            assert!(acting.try_recv().is_err(), "a copy acted again");
            return;
        };
        assert!(
            child.status.success() && String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "a copy of the first signal cut the cleanup short ({}); stderr: {}",
            child.status,
            stderr(&child)
        );
        assert!(
            !stderr(&child).contains("received a second"),
            "{}",
            stderr(&child)
        );
    }

    /// The first signal's action can block: exec's waits for the actor with
    /// no timeout. A second signal, once it can no longer be a copy of the
    /// first, must still end the process at once.
    #[test]
    fn a_second_signal_exits_at_once_while_the_first_ones_action_blocks() {
        let Some(child) = in_a_process_of_its_own(
            "a_second_signal_exits_at_once_while_the_first_ones_action_blocks",
        ) else {
            let (acting_tx, acting) = mpsc::channel();
            let _signals = handle_termination_signals(
                &[TerminationSignal::Interrupt, TerminationSignal::Terminate],
                Arc::new(AtomicBool::new(false)),
                move |_| {
                    let _ = acting_tx.send(());
                    loop {
                        std::thread::park();
                    }
                },
                || {},
            )
            .expect("the signal handlers");
            raise(libc::SIGTERM);
            acting
                .recv_timeout(Duration::from_secs(1))
                .expect("the first signal's action started");
            wait_out_the_copies();
            raise(libc::SIGINT);
            wait_to_be_ended();
        };
        assert_eq!(
            child.status.code(),
            Some(TerminationSignal::Interrupt.exit_code()),
            "the second signal did not end the process ({}); stderr: {}",
            child.status,
            stderr(&child)
        );
        assert!(
            stderr(&child).contains("orca: received a second SIGINT; exiting immediately"),
            "{}",
            stderr(&child)
        );
    }

    /// Raises SIGTERM twice, the second once the first one's action has
    /// started and it can no longer be a copy of the first, with
    /// `before_exit` for the exit the second one forces.
    fn force_an_exit_with(before_exit: impl FnOnce() + Send + 'static) -> ! {
        let (acting_tx, acting) = mpsc::channel();
        let _signals = handle_termination_signals(
            &[TerminationSignal::Terminate],
            Arc::new(AtomicBool::new(false)),
            move |_| {
                let _ = acting_tx.send(());
            },
            before_exit,
        )
        .expect("the signal handlers");
        raise(libc::SIGTERM);
        acting
            .recv_timeout(Duration::from_secs(1))
            .expect("the first signal's action ran");
        wait_out_the_copies();
        let _ = writeln!(
            std::io::stderr(),
            "{FORCING_AT}{}",
            monotonic_now().as_nanos()
        );
        raise(libc::SIGTERM);
        wait_to_be_ended();
    }

    /// A forced exit gives the caller its chance first: the TUI puts the
    /// terminal back before the line that says why.
    #[test]
    fn before_exit_runs_before_the_exit_line() {
        const RAN: &str = "before_exit ran";
        let Some(child) = in_a_process_of_its_own("before_exit_runs_before_the_exit_line") else {
            force_an_exit_with(|| {
                let _ = writeln!(std::io::stderr(), "{RAN}");
            });
        };
        let stderr = stderr(&child);
        assert_eq!(
            child.status.code(),
            Some(TerminationSignal::Terminate.exit_code()),
            "the second signal did not end the process ({}); stderr: {stderr}",
            child.status
        );
        let ran = stderr.find(RAN);
        let exit_line = stderr.find("orca: received a second SIGTERM; exiting immediately");
        assert!(
            ran.is_some() && exit_line.is_some() && ran < exit_line,
            "before_exit did not run before the exit line: {stderr}"
        );
    }

    /// Once the cleanup finished, nothing is cut short: a later signal
    /// neither ends the process nor calls `before_exit`. The signal comes
    /// just after `finished` is set, to find it set; but the grace period's
    /// poll, which ends the watch once `finished` is set, now and then gets
    /// there first, and then the signal finds no one. The child tries a few
    /// times, each with a handler of its own.
    #[test]
    fn before_exit_never_runs_once_the_cleanup_finished() {
        const TRIES: usize = 3;
        let Some(child) =
            in_a_process_of_its_own("before_exit_never_runs_once_the_cleanup_finished")
        else {
            let before_exit_ran = Arc::new(AtomicBool::new(false));
            for _ in 0..TRIES {
                let finished = Arc::new(AtomicBool::new(false));
                let (acting_tx, acting) = mpsc::channel();
                let _signals = handle_termination_signals(
                    &[TerminationSignal::Terminate],
                    Arc::clone(&finished),
                    move |_| {
                        let _ = acting_tx.send(());
                    },
                    {
                        let before_exit_ran = Arc::clone(&before_exit_ran);
                        move || before_exit_ran.store(true, Ordering::SeqCst)
                    },
                )
                .expect("the signal handlers");
                raise(libc::SIGTERM);
                acting
                    .recv_timeout(Duration::from_secs(1))
                    .expect("the first signal's action ran");
                wait_out_the_copies();
                finished.store(true, Ordering::SeqCst);
                raise(libc::SIGTERM);
                // Longer than the grace period's poll: this watch is over.
                std::thread::sleep(Duration::from_millis(100));
            }
            // Longer than a forced exit's wait for before_exit.
            std::thread::sleep(BEFORE_EXIT_BOUND + Duration::from_millis(100));
            assert!(!before_exit_ran.load(Ordering::SeqCst));
            return;
        };
        assert!(
            child.status.success() && String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "a signal after the cleanup finished cut it short ({}); stderr: {}",
            child.status,
            stderr(&child)
        );
    }

    /// A `before_exit` that never returns (a terminal write that blocks, a
    /// lock the renderer holds) cannot defeat the exit it comes before: the
    /// exit waits for it BEFORE_EXIT_BOUND, and no longer.
    #[test]
    fn a_before_exit_that_never_returns_delays_the_exit_by_its_bound_only() {
        let Some(child) = in_a_process_of_its_own(
            "a_before_exit_that_never_returns_delays_the_exit_by_its_bound_only",
        ) else {
            force_an_exit_with(|| {
                loop {
                    std::thread::park();
                }
            });
        };
        let exited_by = monotonic_now();
        let stderr = stderr(&child);
        assert_eq!(
            child.status.code(),
            Some(TerminationSignal::Terminate.exit_code()),
            "the exit waited for before_exit ({}); stderr: {stderr}",
            child.status
        );
        assert!(
            stderr.contains("orca: received a second SIGTERM; exiting immediately"),
            "{stderr}"
        );
        let forced_at = stderr
            .lines()
            .find_map(|line| line.strip_prefix(FORCING_AT))
            .and_then(|nanos| nanos.trim().parse::<u64>().ok())
            .map(Duration::from_nanos)
            .unwrap_or_else(|| panic!("the child did not say when it forced its exit: {stderr}"));
        let waited = exited_by.saturating_sub(forced_at);
        assert!(
            waited >= BEFORE_EXIT_BOUND && waited < Duration::from_millis(1500),
            "the exit came {waited:?} after the signal that forced it, not just past \
             the {BEFORE_EXIT_BOUND:?} it waits for before_exit"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::process::CommandExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{TerminationSignal, handle_termination_signals};

    /// The console's Ctrl+Break, as `GenerateConsoleCtrlEvent` names it.
    const CTRL_BREAK_EVENT: u32 = 1;
    /// Starts a process as the leader of a process group of its own, which a
    /// console event can be sent to alone.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AllocConsole() -> i32;
        fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    }

    /// A job runner stops a console process group with Ctrl+Break, which
    /// asks the process to terminate, as SIGTERM does. The test sends it to a
    /// process group of its own: the test binary run again for just this
    /// test.
    #[test]
    fn ctrl_break_asks_the_process_to_terminate() {
        const CHILD_ENV: &str = "ORCA_TEST_CONSOLE_EVENT_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let home = tempfile::tempdir().expect("an Orca home for the child");
            let output = std::process::Command::new(
                std::env::current_exe().expect("test executable"),
            )
            .args([
                "--exact",
                "termination_signals::windows_tests::ctrl_break_asks_the_process_to_terminate",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .env("ORCA_HOME", home.path())
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .output()
            .expect("run the test in a process group of its own");
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "Ctrl+Break was not taken for a request to terminate ({}); stdout: {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // A process started with no console gets one of its own; one that
        // has one keeps it.
        unsafe { AllocConsole() };
        let finished = Arc::new(AtomicBool::new(false));
        let (acting_tx, acting) = mpsc::channel();
        let _signals = handle_termination_signals(
            &[TerminationSignal::Interrupt, TerminationSignal::Terminate],
            Arc::clone(&finished),
            move |signal| {
                let _ = acting_tx.send(signal);
            },
            || {},
        )
        .expect("the console event handlers");
        // The child leads its process group, so the event reaches it alone.
        assert_ne!(
            unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, std::process::id()) },
            0,
            "send Ctrl+Break: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            acting
                .recv_timeout(Duration::from_secs(5))
                .expect("the event's action ran"),
            TerminationSignal::Terminate
        );
        finished.store(true, Ordering::SeqCst);
    }
}
