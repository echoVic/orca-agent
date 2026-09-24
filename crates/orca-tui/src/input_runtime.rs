use std::error::Error as _;
use std::io;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_core::config::ThemeName;
use qwertty::{
    Event as QwerttyEvent, KittyKeyboardFlags, MouseMode, RestoreHandle, Rgb, TokioTerminalSession,
};
use tokio::sync::watch;

use crate::input_adapter::InputAdapter;
use crate::terminal_capabilities::{
    TerminalColorLevel, TerminalProfile, system_color_level, terminal_background_from_rgb,
};

const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const INPUT_CHANNEL_CAPACITY: usize = 256;
const FOCUS_CHANNEL_CAPACITY: usize = 8;
const STARTUP_CHANNEL_CAPACITY: usize = 1;

static PANIC_HOOK_INIT: Once = Once::new();
static ACTIVE_RESTORE_HANDLE: Mutex<Option<ActiveRestore>> = Mutex::new(None);
static TERMINAL_OWNER_ACTIVE: AtomicBool = AtomicBool::new(false);

struct ActiveRestore {
    handle: RestoreHandle,
    owner_thread: thread::ThreadId,
    input_thread: thread::ThreadId,
}

fn should_restore_for_panic(
    owner_thread: thread::ThreadId,
    input_thread: thread::ThreadId,
    panicking_thread: thread::ThreadId,
) -> bool {
    panicking_thread == owner_thread || panicking_thread == input_thread
}

struct TerminalOwnerLease;

impl TerminalOwnerLease {
    fn acquire() -> io::Result<Self> {
        if TERMINAL_OWNER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "another TUI already owns the terminal",
            ));
        }
        Ok(Self)
    }
}

impl Drop for TerminalOwnerLease {
    fn drop(&mut self) {
        TERMINAL_OWNER_ACTIVE.store(false, Ordering::Release);
    }
}

enum StartupMessage {
    Ready(TerminalProfile),
    Failed {
        kind: io::ErrorKind,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InputRuntimeOptions {
    pub(crate) theme: ThemeName,
    pub(crate) focus_events: bool,
}

impl From<ThemeName> for InputRuntimeOptions {
    fn from(theme: ThemeName) -> Self {
        Self {
            theme,
            focus_events: false,
        }
    }
}

pub(crate) enum InputControl {
    Suspend {
        acknowledge: tokio::sync::oneshot::Sender<()>,
    },
    Resumed,
}

struct InputEventSender {
    events: mpsc::Sender<crossterm::event::Event>,
    focus_events: Option<mpsc::Sender<crossterm::event::Event>>,
}

impl InputEventSender {
    fn with_focus(
        events: mpsc::Sender<crossterm::event::Event>,
        focus_events: mpsc::Sender<crossterm::event::Event>,
    ) -> Self {
        Self {
            events,
            focus_events: Some(focus_events),
        }
    }

    fn sender_for(
        &self,
        event: &crossterm::event::Event,
    ) -> &mpsc::Sender<crossterm::event::Event> {
        if matches!(
            event,
            crossterm::event::Event::FocusGained | crossterm::event::Event::FocusLost
        ) {
            self.focus_events.as_ref().unwrap_or(&self.events)
        } else {
            &self.events
        }
    }
}

impl From<mpsc::Sender<crossterm::event::Event>> for InputEventSender {
    fn from(events: mpsc::Sender<crossterm::event::Event>) -> Self {
        Self {
            events,
            focus_events: None,
        }
    }
}

pub(crate) struct InputRuntime {
    profile: TerminalProfile,
    events: mpsc::Receiver<crossterm::event::Event>,
    focus_events: mpsc::Receiver<crossterm::event::Event>,
    controls: mpsc::Receiver<InputControl>,
    stop_tx: Option<watch::Sender<bool>>,
    join: Option<thread::JoinHandle<io::Result<()>>>,
    owner_lease: Option<TerminalOwnerLease>,
}

impl InputRuntime {
    pub(crate) fn start(options: InputRuntimeOptions) -> io::Result<Self> {
        let owner_lease = TerminalOwnerLease::acquire()?;
        let owner_thread = thread::current().id();

        let color_level = system_color_level();
        let (event_tx, events) = mpsc::bounded(INPUT_CHANNEL_CAPACITY);
        let (focus_tx, focus_events) = mpsc::bounded(FOCUS_CHANNEL_CAPACITY);
        let (control_tx, controls) = mpsc::bounded(1);
        let (startup_tx, startup_rx) = mpsc::bounded(STARTUP_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = watch::channel(false);
        let join = match thread::Builder::new()
            .name("orca-tui-input".to_string())
            .spawn(move || {
                input_thread(
                    options,
                    color_level,
                    startup_tx,
                    InputEventSender::with_focus(event_tx, focus_tx),
                    control_tx,
                    stop_rx,
                    owner_thread,
                )
            }) {
            Ok(join) => join,
            Err(error) => return Err(error),
        };

        match startup_rx.recv() {
            Ok(StartupMessage::Ready(profile)) => Ok(Self {
                profile,
                events,
                focus_events,
                controls,
                stop_tx: Some(stop_tx),
                join: Some(join),
                owner_lease: Some(owner_lease),
            }),
            Ok(StartupMessage::Failed { kind, message }) => {
                let _ = join.join();
                Err(io::Error::new(kind, message))
            }
            Err(_) => {
                let thread_error = match join.join() {
                    Ok(result) => result.err(),
                    Err(_) => Some(io::Error::other("terminal input thread panicked")),
                };
                Err(thread_error.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input thread exited before startup completed",
                    )
                }))
            }
        }
    }

    pub(crate) const fn profile(&self) -> TerminalProfile {
        self.profile
    }

    pub(crate) fn events(&self) -> &mpsc::Receiver<crossterm::event::Event> {
        &self.events
    }

    pub(crate) fn focus_events(&self) -> &mpsc::Receiver<crossterm::event::Event> {
        &self.focus_events
    }

    pub(crate) fn controls(&self) -> &mpsc::Receiver<InputControl> {
        &self.controls
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(true);
        }
        let result = if let Some(join) = self.join.take() {
            match join.join() {
                Ok(result) => result,
                Err(_) => Err(io::Error::other("terminal input thread panicked")),
            }
        } else {
            Ok(())
        };
        self.owner_lease.take();
        result
    }

    #[cfg(test)]
    fn from_parts_for_test(
        profile: TerminalProfile,
        events: mpsc::Receiver<crossterm::event::Event>,
        controls: mpsc::Receiver<InputControl>,
        stop_tx: watch::Sender<bool>,
        join: thread::JoinHandle<io::Result<()>>,
    ) -> Self {
        let (_focus_tx, focus_events) = mpsc::bounded(1);
        Self {
            profile,
            events,
            focus_events,
            controls,
            stop_tx: Some(stop_tx),
            join: Some(join),
            owner_lease: Some(TerminalOwnerLease),
        }
    }
}

impl Drop for InputRuntime {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn input_thread(
    options: InputRuntimeOptions,
    color_level: TerminalColorLevel,
    startup_tx: mpsc::Sender<StartupMessage>,
    event_tx: InputEventSender,
    control_tx: mpsc::Sender<InputControl>,
    stop_rx: watch::Receiver<bool>,
    owner_thread: thread::ThreadId,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (driver, restore_handle) = match QwerttyDriver::open() {
            Ok(driver) => driver,
            Err(error) => {
                let _ = startup_tx.send(StartupMessage::Failed {
                    kind: error.kind(),
                    message: error.to_string(),
                });
                return Err(error);
            }
        };
        let _restore_registration = RestoreRegistration::install(restore_handle, owner_thread)?;
        drive_terminal(
            driver,
            options,
            color_level,
            startup_tx,
            event_tx,
            control_tx,
            stop_rx,
        )
        .await
    })
}

struct RestoreRegistration;

impl RestoreRegistration {
    fn install(handle: RestoreHandle, owner_thread: thread::ThreadId) -> io::Result<Self> {
        PANIC_HOOK_INIT.call_once(|| {
            let previous = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                if let Ok(slot) = ACTIVE_RESTORE_HANDLE.try_lock()
                    && let Some(active) = slot.as_ref()
                    && should_restore_for_panic(
                        active.owner_thread,
                        active.input_thread,
                        thread::current().id(),
                    )
                {
                    let _ = active.handle.restore();
                }
                previous(info);
            }));
        });
        let mut slot = ACTIVE_RESTORE_HANDLE
            .lock()
            .map_err(|_| io::Error::other("terminal restore registry is poisoned"))?;
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal restore handle is already registered",
            ));
        }
        *slot = Some(ActiveRestore {
            handle,
            owner_thread,
            input_thread: thread::current().id(),
        });
        Ok(Self)
    }
}

impl Drop for RestoreRegistration {
    fn drop(&mut self) {
        if let Ok(mut slot) = ACTIVE_RESTORE_HANDLE.lock() {
            *slot = None;
        }
    }
}

/// qwertty reads the terminal through `dup`s of it that every spawned child
/// would otherwise inherit, holding the terminal open past the session: a
/// detached subagent worker kept a finished TUI's terminal alive this way.
/// Failing to mark them only risks that leak, so it is not an error.
fn keep_terminal_out_of_children() {
    let _ = orca_platform::terminal::keep_terminal_descriptors_out_of_children();
}

struct QwerttyDriver {
    session: TokioTerminalSession,
    signals: qwertty::SignalStream,
    #[cfg(unix)]
    resizes: qwertty::ResizeStream,
}

impl QwerttyDriver {
    fn open() -> io::Result<(Self, RestoreHandle)> {
        let session = TokioTerminalSession::open().map_err(qwertty_error)?;
        let restore_handle = session.restore_handle();
        let signals = session.signals().map_err(qwertty_error)?;
        #[cfg(unix)]
        let resizes = session.resize_stream().map_err(qwertty_error)?;
        keep_terminal_out_of_children();
        Ok((
            Self {
                session,
                signals,
                #[cfg(unix)]
                resizes,
            },
            restore_handle,
        ))
    }
}

#[derive(Clone, Debug)]
enum TerminalActivity {
    Event(QwerttyEvent),
    Suspend,
    Continue,
    Terminate,
    Ignore,
}

trait TerminalDriver: Sized {
    async fn probe_background(&mut self, timeout: Duration) -> io::Result<Option<Rgb>>;
    async fn enter_alternate_screen(&mut self) -> io::Result<()>;
    async fn enable_mouse(&mut self) -> io::Result<()>;
    async fn enable_bracketed_paste(&mut self) -> io::Result<()>;
    async fn enable_focus_events(&mut self) -> io::Result<()>;
    async fn push_keyboard_flags(&mut self) -> io::Result<()>;
    async fn next_activity(&mut self) -> io::Result<TerminalActivity>;
    async fn suspend(&mut self) -> io::Result<()>;
    async fn resume(&mut self) -> io::Result<()>;
    async fn leave(self) -> io::Result<()>;
}

impl TerminalDriver for QwerttyDriver {
    async fn probe_background(&mut self, timeout: Duration) -> io::Result<Option<Rgb>> {
        self.session
            .probe_capabilities(timeout)
            .await
            .map(|capabilities| capabilities.background_color.value_copied())
            .map_err(qwertty_error)
    }

    async fn enter_alternate_screen(&mut self) -> io::Result<()> {
        self.session
            .enter_alternate_screen()
            .await
            .map_err(qwertty_error)
    }

    async fn enable_mouse(&mut self) -> io::Result<()> {
        self.session
            .enable_mouse(MouseMode::ButtonEvent)
            .await
            .map_err(qwertty_error)
    }

    async fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        self.session
            .enable_bracketed_paste()
            .await
            .map_err(qwertty_error)
    }

    async fn enable_focus_events(&mut self) -> io::Result<()> {
        self.session
            .enable_focus_events()
            .await
            .map_err(qwertty_error)
    }

    async fn push_keyboard_flags(&mut self) -> io::Result<()> {
        let flags = KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
            .union(KittyKeyboardFlags::REPORT_EVENT_TYPES)
            .union(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS);
        self.session
            .push_kitty_keyboard(flags)
            .await
            .map_err(qwertty_error)
    }

    async fn next_activity(&mut self) -> io::Result<TerminalActivity> {
        #[cfg(unix)]
        {
            tokio::select! {
                biased;
                signal = self.signals.next() => {
                    signal.map(terminal_signal_activity).map_err(qwertty_error)
                }
                resize = self.resizes.next_resize() => {
                    resize
                        .map(QwerttyEvent::Resize)
                        .map(TerminalActivity::Event)
                        .map_err(qwertty_error)
                }
                event = self.session.next_event() => {
                    event.map(TerminalActivity::Event).map_err(qwertty_error)
                },
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                biased;
                signal = self.signals.next() => {
                    signal.map(terminal_signal_activity).map_err(qwertty_error)
                }
                event = self.session.next_event() => {
                    event.map(TerminalActivity::Event).map_err(qwertty_error)
                }
            }
        }
    }

    async fn suspend(&mut self) -> io::Result<()> {
        self.session.suspend().await.map_err(qwertty_error)
    }

    async fn resume(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let resumed = self.session.resume(false).await.map_err(qwertty_error);
            // Resuming re-registers the reader on a fresh duplicate.
            keep_terminal_out_of_children();
            resumed
        }
        #[cfg(windows)]
        {
            Ok(())
        }
    }

    async fn leave(self) -> io::Result<()> {
        self.session.leave().await.map_err(qwertty_error)
    }
}

fn terminal_signal_activity(signal: qwertty::TerminalSignal) -> TerminalActivity {
    match signal {
        qwertty::TerminalSignal::Suspend => TerminalActivity::Suspend,
        qwertty::TerminalSignal::Continue => TerminalActivity::Continue,
        qwertty::TerminalSignal::Terminate | qwertty::TerminalSignal::Interrupt => {
            TerminalActivity::Terminate
        }
        _ => TerminalActivity::Ignore,
    }
}

fn qwertty_error(error: qwertty::Error) -> io::Error {
    let kind = error
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .map_or(io::ErrorKind::Other, io::Error::kind);
    io::Error::new(kind, error)
}

async fn drive_terminal<D: TerminalDriver>(
    mut driver: D,
    options: impl Into<InputRuntimeOptions>,
    color_level: TerminalColorLevel,
    startup_tx: mpsc::Sender<StartupMessage>,
    event_tx: impl Into<InputEventSender>,
    control_tx: mpsc::Sender<InputControl>,
    mut stop_rx: watch::Receiver<bool>,
) -> io::Result<()> {
    let options = options.into();
    let background = if options.theme == ThemeName::Auto {
        match driver.probe_background(CAPABILITY_PROBE_TIMEOUT).await {
            Ok(background) => background,
            Err(error) => {
                return fail_startup(driver, startup_tx, error).await;
            }
        }
    } else {
        None
    };

    if let Err(error) = driver.enter_alternate_screen().await {
        return fail_startup(driver, startup_tx, error).await;
    }
    if let Err(error) = driver.enable_mouse().await {
        return fail_startup(driver, startup_tx, error).await;
    }
    if let Err(error) = driver.enable_bracketed_paste().await {
        return fail_startup(driver, startup_tx, error).await;
    }
    if options.focus_events
        && let Err(error) = driver.enable_focus_events().await
    {
        return fail_startup(driver, startup_tx, error).await;
    }
    if let Err(error) = driver.push_keyboard_flags().await {
        return fail_startup(driver, startup_tx, error).await;
    }

    let profile = TerminalProfile {
        background: terminal_background_from_rgb(options.theme, background),
        color_level,
    };
    if startup_tx.send(StartupMessage::Ready(profile)).is_err() {
        return finish_driver(
            driver,
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI startup receiver closed",
            )),
        )
        .await;
    }

    let mut adapter = InputAdapter::default();
    let mut event_tx = Some(event_tx.into());
    let mut control_tx = Some(control_tx);
    let mut wait_for_main_teardown = false;
    let mut suspended = false;
    let operation = loop {
        tokio::select! {
            biased;
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if *stop_rx.borrow() => break Ok(()),
                    Ok(()) => {}
                    Err(_) => break Ok(()),
                }
            }
            activity = driver.next_activity() => {
                let activity = match activity {
                    Ok(activity) => activity,
                    Err(error) => {
                        wait_for_main_teardown = true;
                        break Err(error);
                    }
                };
                match activity {
                    TerminalActivity::Event(event) => {
                        if let Some(event) = adapter.adapt(event) {
                            let sender = event_tx
                                .as_ref()
                                .expect("event sender is live")
                                .sender_for(&event);
                            if !send_event_until_stopped(
                                sender,
                                event,
                                &mut stop_rx,
                            )
                            .await
                            {
                                break Ok(());
                            }
                        }
                    }
                    TerminalActivity::Suspend => {
                        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
                        if !send_control_until_stopped(
                            control_tx.as_ref().expect("control sender is live"),
                            InputControl::Suspend { acknowledge },
                            &mut stop_rx,
                        )
                        .await
                        {
                            break Ok(());
                        }
                        tokio::select! {
                            biased;
                            changed = stop_rx.changed() => {
                                match changed {
                                    Ok(()) if *stop_rx.borrow() => break Ok(()),
                                    Ok(()) => continue,
                                    Err(_) => break Ok(()),
                                }
                            }
                            acknowledged = acknowledged => {
                                if acknowledged.is_err() {
                                    wait_for_main_teardown = true;
                                    break Err(io::Error::new(
                                        io::ErrorKind::BrokenPipe,
                                        "TUI suspend acknowledgement closed",
                                    ));
                                }
                            }
                        }
                        if let Err(error) = driver.suspend().await {
                            wait_for_main_teardown = true;
                            break Err(error);
                        }
                        suspended = true;
                    }
                    TerminalActivity::Continue if suspended => {
                        if let Err(error) = driver.resume().await {
                            wait_for_main_teardown = true;
                            break Err(error);
                        }
                        suspended = false;
                        if !send_control_until_stopped(
                            control_tx.as_ref().expect("control sender is live"),
                            InputControl::Resumed,
                            &mut stop_rx,
                        )
                        .await
                        {
                            break Ok(());
                        }
                    }
                    TerminalActivity::Continue => {}
                    TerminalActivity::Terminate => {
                        wait_for_main_teardown = true;
                        break Ok(());
                    }
                    TerminalActivity::Ignore => {}
                }
            }
        }
    };

    if wait_for_main_teardown {
        event_tx.take();
        control_tx.take();
        wait_for_stop(&mut stop_rx).await;
    }
    finish_driver(driver, operation).await
}

async fn wait_for_stop(stop_rx: &mut watch::Receiver<bool>) {
    while !*stop_rx.borrow() {
        if stop_rx.changed().await.is_err() {
            break;
        }
    }
}

async fn fail_startup<D: TerminalDriver>(
    driver: D,
    startup_tx: mpsc::Sender<StartupMessage>,
    error: io::Error,
) -> io::Result<()> {
    let kind = error.kind();
    let message = error.to_string();
    let result = finish_driver(driver, Err(error)).await;
    let _ = startup_tx.send(StartupMessage::Failed { kind, message });
    result
}

async fn finish_driver<D: TerminalDriver>(driver: D, operation: io::Result<()>) -> io::Result<()> {
    let leave = driver.leave().await;
    operation.and(leave)
}

async fn send_event_until_stopped(
    sender: &mpsc::Sender<crossterm::event::Event>,
    mut event: crossterm::event::Event,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(returned)) => event = returned,
        }

        tokio::select! {
            biased;
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if *stop_rx.borrow() => return false,
                    Ok(()) => {}
                    Err(_) => return false,
                }
            }
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    }
}

async fn send_control_until_stopped(
    sender: &mpsc::Sender<InputControl>,
    mut control: InputControl,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        match sender.try_send(control) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(returned)) => control = returned,
        }

        tokio::select! {
            biased;
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if *stop_rx.borrow() => return false,
                    Ok(()) => {}
                    Err(_) => return false,
                }
            }
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;
    use std::time::Duration;

    use crossbeam_channel as mpsc;
    use orca_core::config::ThemeName;
    use qwertty::{Event, Key, KeyEvent, Rgb};
    use tokio::sync::watch;

    use super::{
        InputControl, InputRuntimeOptions, StartupMessage, TERMINAL_OWNER_ACTIVE, TerminalActivity,
        TerminalDriver, drive_terminal, should_restore_for_panic,
    };
    use crate::terminal_capabilities::{TerminalBackground, TerminalColorLevel};

    static OWNER_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct FakeDriver {
        calls: Arc<Mutex<Vec<&'static str>>>,
        background: Option<Rgb>,
        activities: VecDeque<TerminalActivity>,
        fail_at: Option<&'static str>,
    }

    impl FakeDriver {
        fn new(
            calls: Arc<Mutex<Vec<&'static str>>>,
            background: Option<Rgb>,
            events: impl IntoIterator<Item = Event>,
        ) -> Self {
            Self {
                calls,
                background,
                activities: events.into_iter().map(TerminalActivity::Event).collect(),
                fail_at: None,
            }
        }

        fn with_activities(
            calls: Arc<Mutex<Vec<&'static str>>>,
            activities: impl IntoIterator<Item = TerminalActivity>,
        ) -> Self {
            Self {
                calls,
                background: None,
                activities: activities.into_iter().collect(),
                fail_at: None,
            }
        }

        fn failing(mut self, operation: &'static str) -> Self {
            self.fail_at = Some(operation);
            self
        }

        fn record(&self, operation: &'static str) -> io::Result<()> {
            self.calls.lock().expect("calls lock").push(operation);
            if self.fail_at == Some(operation) {
                Err(io::Error::other(format!("{operation} failed")))
            } else {
                Ok(())
            }
        }
    }

    impl TerminalDriver for FakeDriver {
        async fn probe_background(&mut self, _timeout: Duration) -> io::Result<Option<Rgb>> {
            self.record("probe")?;
            Ok(self.background)
        }

        async fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.record("alternate")
        }

        async fn enable_mouse(&mut self) -> io::Result<()> {
            self.record("mouse")
        }

        async fn enable_bracketed_paste(&mut self) -> io::Result<()> {
            self.record("paste")
        }

        async fn enable_focus_events(&mut self) -> io::Result<()> {
            self.record("focus")
        }

        async fn push_keyboard_flags(&mut self) -> io::Result<()> {
            self.record("keyboard")
        }

        async fn next_activity(&mut self) -> io::Result<TerminalActivity> {
            self.record("read")?;
            if let Some(activity) = self.activities.pop_front() {
                Ok(activity)
            } else {
                std::future::pending().await
            }
        }

        async fn suspend(&mut self) -> io::Result<()> {
            self.record("suspend")
        }

        async fn resume(&mut self) -> io::Result<()> {
            self.record("resume")
        }

        async fn leave(self) -> io::Result<()> {
            self.record("leave")
        }
    }

    fn light_background() -> Option<Rgb> {
        Some(Rgb::new(255, 255, 255))
    }

    async fn wait_for_startup(receiver: &mpsc::Receiver<StartupMessage>) -> StartupMessage {
        for _ in 0..100 {
            if let Ok(message) = receiver.try_recv() {
                return message;
            }
            tokio::task::yield_now().await;
        }
        panic!("startup message was not sent");
    }

    async fn wait_for_control(receiver: &mpsc::Receiver<InputControl>) -> InputControl {
        for _ in 0..100 {
            if let Ok(control) = receiver.try_recv() {
                return control;
            }
            tokio::task::yield_now().await;
        }
        panic!("control message was not sent");
    }

    #[test]
    fn auto_probe_precedes_modes_ready_reads_and_leave() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), light_background(), []);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(4);
                let (control_tx, _control_rx) = mpsc::bounded(1);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Auto,
                    TerminalColorLevel::Ansi256,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                ));
                let StartupMessage::Ready(profile) = wait_for_startup(&startup_rx).await else {
                    panic!("expected ready startup message");
                };
                assert_eq!(profile.background, TerminalBackground::Light);
                assert_eq!(profile.color_level, TerminalColorLevel::Ansi256);
                stop_tx.send(true).expect("stop receiver alive");
                task.await.expect("driver task").expect("clean stop");

                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    [
                        "probe",
                        "alternate",
                        "mouse",
                        "paste",
                        "keyboard",
                        "read",
                        "leave"
                    ]
                );
            });
    }

    #[test]
    fn explicit_theme_skips_probe_but_keeps_mode_order() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), light_background(), []);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(4);
                let (control_tx, _control_rx) = mpsc::bounded(1);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Light,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                ));
                assert!(matches!(
                    wait_for_startup(&startup_rx).await,
                    StartupMessage::Ready(_)
                ));
                stop_tx.send(true).expect("stop receiver alive");
                task.await.expect("driver task").expect("clean stop");

                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    ["alternate", "mouse", "paste", "keyboard", "read", "leave"]
                );
            });
    }

    #[test]
    fn focus_events_are_enabled_only_when_requested_before_keyboard_flags() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                for (focus_events, expected) in [
                    (
                        true,
                        vec![
                            "alternate",
                            "mouse",
                            "paste",
                            "focus",
                            "keyboard",
                            "read",
                            "leave",
                        ],
                    ),
                    (
                        false,
                        vec!["alternate", "mouse", "paste", "keyboard", "read", "leave"],
                    ),
                ] {
                    let calls = Arc::new(Mutex::new(Vec::new()));
                    let driver = FakeDriver::new(Arc::clone(&calls), None, []);
                    let (startup_tx, startup_rx) = mpsc::bounded(1);
                    let (event_tx, _event_rx) = mpsc::bounded(1);
                    let (control_tx, _control_rx) = mpsc::bounded(1);
                    let (stop_tx, stop_rx) = watch::channel(false);

                    let task = tokio::spawn(drive_terminal(
                        driver,
                        InputRuntimeOptions {
                            theme: ThemeName::Dark,
                            focus_events,
                        },
                        TerminalColorLevel::TrueColor,
                        startup_tx,
                        event_tx,
                        control_tx,
                        stop_rx,
                    ));
                    let _ = wait_for_startup(&startup_rx).await;
                    stop_tx.send(true).expect("stop receiver alive");
                    task.await.expect("driver task").expect("clean stop");

                    assert_eq!(*calls.lock().expect("calls lock"), expected);
                }
            });
    }

    #[test]
    fn full_mailbox_never_blocks_stop_or_leave() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let events = [
                    Event::Key(KeyEvent::new(Key::Char('a'))),
                    Event::Key(KeyEvent::new(Key::Char('b'))),
                ];
                let driver = FakeDriver::new(Arc::clone(&calls), None, events);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, event_rx) = mpsc::bounded(1);
                let (control_tx, _control_rx) = mpsc::bounded(1);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                ));
                let _ = wait_for_startup(&startup_rx).await;
                for _ in 0..100 {
                    if event_rx.len() == 1
                        && calls
                            .lock()
                            .expect("calls lock")
                            .iter()
                            .filter(|call| **call == "read")
                            .count()
                            >= 2
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                stop_tx.send(true).expect("stop receiver alive");
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .expect("stop must not deadlock")
                    .expect("driver task")
                    .expect("clean stop");

                assert_eq!(calls.lock().expect("calls lock").last(), Some(&"leave"));
            });
    }

    #[test]
    fn full_control_mailbox_never_blocks_stop_or_leave() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver =
                    FakeDriver::with_activities(Arc::clone(&calls), [TerminalActivity::Suspend]);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(1);
                let (control_tx, control_rx) = mpsc::bounded(1);
                control_tx
                    .send(InputControl::Resumed)
                    .expect("control receiver alive");
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                ));
                let _ = wait_for_startup(&startup_rx).await;
                for _ in 0..100 {
                    if calls.lock().expect("calls lock").contains(&"read") {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                stop_tx.send(true).expect("stop receiver alive");
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .expect("stop must interrupt a full control mailbox")
                    .expect("driver task")
                    .expect("clean stop");

                drop(control_rx);
                assert_eq!(calls.lock().expect("calls lock").last(), Some(&"leave"));
            });
    }

    #[test]
    fn startup_failure_is_reported_and_still_leaves() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), None, []).failing("mouse");
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(1);
                let (control_tx, _control_rx) = mpsc::bounded(1);
                let (_stop_tx, stop_rx) = watch::channel(false);

                let error = drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                )
                .await
                .expect_err("mode failure should fail startup");
                assert_eq!(error.to_string(), "mouse failed");
                assert!(matches!(
                    startup_rx.recv().expect("startup error"),
                    StartupMessage::Failed { .. }
                ));
                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    ["alternate", "mouse", "leave"]
                );
            });
    }

    #[test]
    fn terminal_signals_suspend_resume_and_terminate_through_qwertty() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::with_activities(
                    Arc::clone(&calls),
                    [
                        TerminalActivity::Suspend,
                        TerminalActivity::Continue,
                        TerminalActivity::Terminate,
                    ],
                );
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, event_rx) = mpsc::bounded(1);
                let (control_tx, control_rx) = mpsc::bounded(1);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                ));
                assert!(matches!(
                    wait_for_startup(&startup_rx).await,
                    StartupMessage::Ready(_)
                ));
                let InputControl::Suspend { acknowledge } = wait_for_control(&control_rx).await
                else {
                    panic!("expected suspend control");
                };
                acknowledge.send(()).expect("acknowledge suspend");
                assert!(matches!(
                    wait_for_control(&control_rx).await,
                    InputControl::Resumed
                ));
                for _ in 0..100 {
                    if matches!(event_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert!(matches!(
                    event_rx.try_recv(),
                    Err(mpsc::TryRecvError::Disconnected)
                ));
                stop_tx.send(true).expect("stop receiver alive");
                task.await.expect("driver task").expect("clean signal exit");

                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    [
                        "alternate",
                        "mouse",
                        "paste",
                        "keyboard",
                        "read",
                        "suspend",
                        "read",
                        "resume",
                        "read",
                        "leave"
                    ]
                );
            });
    }

    #[test]
    fn closed_startup_receiver_still_leaves() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), None, []);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                drop(startup_rx);
                let (event_tx, _event_rx) = mpsc::bounded(1);
                let (control_tx, _control_rx) = mpsc::bounded(1);
                let (_stop_tx, stop_rx) = watch::channel(false);

                let error = drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    control_tx,
                    stop_rx,
                )
                .await
                .expect_err("closed startup receiver should stop the driver");
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    ["alternate", "mouse", "paste", "keyboard", "leave"]
                );
            });
    }

    #[test]
    fn input_runtime_finish_and_drop_signal_and_join_once() {
        let _owner_guard = OWNER_TEST_LOCK.lock().expect("owner test lock");
        for explicit_finish in [true, false] {
            let (stop_tx, mut stop_rx) = watch::channel(false);
            let joined = Arc::new(Mutex::new(0));
            let thread_joined = Arc::clone(&joined);
            let join = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                runtime.block_on(async {
                    stop_rx.changed().await.expect("stop sender alive");
                    assert!(*stop_rx.borrow());
                });
                *thread_joined.lock().expect("join count lock") += 1;
                Ok(())
            });
            let (_event_tx, event_rx) = mpsc::bounded(1);
            let (_control_tx, control_rx) = mpsc::bounded(1);
            let mut runtime = super::InputRuntime::from_parts_for_test(
                super::TerminalProfile {
                    background: TerminalBackground::Unknown,
                    color_level: TerminalColorLevel::TrueColor,
                },
                event_rx,
                control_rx,
                stop_tx,
                join,
            );

            if explicit_finish {
                runtime.finish().expect("finish should join");
                runtime.finish().expect("second finish is idempotent");
            } else {
                drop(runtime);
            }
            assert_eq!(*joined.lock().expect("join count lock"), 1);
        }
    }

    #[test]
    fn input_runtime_thread_panic_releases_global_ownership() {
        let _owner_guard = OWNER_TEST_LOCK.lock().expect("owner test lock");
        TERMINAL_OWNER_ACTIVE.store(true, Ordering::Release);
        let (stop_tx, _stop_rx) = watch::channel(false);
        let (_event_tx, event_rx) = mpsc::bounded(1);
        let (_control_tx, control_rx) = mpsc::bounded(1);
        let join = std::thread::spawn(|| -> io::Result<()> {
            panic!("simulated input thread panic");
        });
        let mut runtime = super::InputRuntime::from_parts_for_test(
            super::TerminalProfile {
                background: TerminalBackground::Unknown,
                color_level: TerminalColorLevel::TrueColor,
            },
            event_rx,
            control_rx,
            stop_tx,
            join,
        );

        let error = runtime.finish().expect_err("thread panic must be reported");
        assert_eq!(error.to_string(), "terminal input thread panicked");
        assert!(!TERMINAL_OWNER_ACTIVE.load(Ordering::Acquire));
    }

    #[test]
    fn finished_runtime_drop_does_not_release_a_replacement_owner() {
        let _owner_guard = OWNER_TEST_LOCK.lock().expect("owner test lock");
        TERMINAL_OWNER_ACTIVE.store(true, Ordering::Release);
        let (stop_tx, _stop_rx) = watch::channel(false);
        let (_event_tx, event_rx) = mpsc::bounded(1);
        let (_control_tx, control_rx) = mpsc::bounded(1);
        let join = std::thread::spawn(|| Ok(()));
        let mut old_runtime = super::InputRuntime::from_parts_for_test(
            super::TerminalProfile {
                background: TerminalBackground::Unknown,
                color_level: TerminalColorLevel::TrueColor,
            },
            event_rx,
            control_rx,
            stop_tx,
            join,
        );

        old_runtime.finish().expect("old runtime should finish");
        assert!(!TERMINAL_OWNER_ACTIVE.load(Ordering::Acquire));
        assert!(
            TERMINAL_OWNER_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "replacement owner should acquire the terminal"
        );

        drop(old_runtime);
        assert!(
            TERMINAL_OWNER_ACTIVE.load(Ordering::Acquire),
            "dropping the finished old runtime must not clear the replacement owner"
        );
        TERMINAL_OWNER_ACTIVE.store(false, Ordering::Release);
    }

    #[test]
    fn panic_restore_is_scoped_to_terminal_owner_threads() {
        let owner = std::thread::current().id();
        let input = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("input thread id");
        let worker = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("worker thread id");

        assert!(should_restore_for_panic(owner, input, owner));
        assert!(should_restore_for_panic(owner, input, input));
        assert!(!should_restore_for_panic(owner, input, worker));

        let _: [ThreadId; 3] = [owner, input, worker];
    }
}
