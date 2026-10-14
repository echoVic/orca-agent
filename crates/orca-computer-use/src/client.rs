use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{SendTimeoutError, Sender};

use crate::protocol::{
    DriverError, DriverEvent, DriverLine, ErrorKind, PROTOCOL_VERSION, Permissions, Reply, Request,
    RequestLine,
};

pub use crate::driver_input::DriverInput;

pub trait DriverLauncher: Send + Sync {
    /// Returns stdin, stdout, and (for a real process) the child to reap.
    fn launch(&self) -> io::Result<LaunchedDriver>;
}

pub struct LaunchedDriver {
    pub stdin: Box<dyn DriverInput>,
    pub stdout: Box<dyn Read + Send>,
    pub child: Option<Child>,
}

pub type EventCallback = Arc<dyn Fn(DriverEvent) + Send + Sync>;
// At most eight queued payloads plus the single frame currently being written.
const OUTBOUND_QUEUE_CAPACITY: usize = 8;

type Pending = Arc<Mutex<HashMap<u64, SyncSender<Result<Reply, DriverError>>>>>;

struct Outbound {
    id: u64,
    request: Request,
    started: Instant,
    timeout: Duration,
}

pub struct DriverClient {
    outgoing: Option<Sender<Outbound>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    stop_writer: Sender<()>,
    pending: Pending,
    connected: Arc<AtomicBool>,
    next_id: AtomicU64,
    reader: Option<JoinHandle<()>>,
    child: Option<Child>,
}

impl fmt::Debug for DriverClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverClient")
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HelloInfo {
    pub macos_major: u32,
    pub host_bundle_id: Option<String>,
    pub permissions: Permissions,
}

#[derive(Debug)]
pub enum CallError {
    Driver(DriverError),
    TimedOut,
    Disconnected,
}

fn failed(message: impl Into<String>) -> DriverError {
    DriverError {
        kind: ErrorKind::Failed,
        message: message.into(),
        candidates: vec![],
    }
}

fn disconnect(pending: &Pending, connected: &AtomicBool) {
    let mut pending = pending.lock().expect("pending replies lock poisoned");
    connected.store(false, Ordering::SeqCst);
    // Dropping the senders wakes every receiver with Disconnected.
    pending.clear();
}

/// Finish a frame before starting another, even if its caller has timed out.
/// Drop interrupts retries; it never leaves a blocked worker holding stdin open.
fn write_request(input: &mut dyn DriverInput, bytes: &[u8], connected: &AtomicBool) -> bool {
    let mut offset = 0;
    while offset < bytes.len() {
        if !connected.load(Ordering::SeqCst) {
            return false;
        }
        match input.write(&bytes[offset..]) {
            Ok(0) => thread::sleep(Duration::from_millis(5)), // A full Windows byte pipe can write zero bytes.
            Ok(written) => offset += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    // Native PipeWriter/ChildStdin flushes are unbuffered and do not block.
    input.flush().is_ok()
}

impl DriverClient {
    /// Launches exactly once and exchanges protocol versions before returning.
    pub fn start(
        launcher: &dyn DriverLauncher,
        on_event: EventCallback,
    ) -> Result<(Self, HelloInfo), DriverError> {
        let launched = launcher
            .launch()
            .map_err(|error| failed(format!("Could not launch computer-use driver: {error}")))?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (stop_writer, stop_receiver) = crossbeam_channel::bounded::<()>(1);
        let mut client = Self {
            outgoing: None,
            writer: Mutex::new(None),
            stop_writer,
            pending: pending.clone(),
            connected: connected.clone(),
            next_id: AtomicU64::new(1),
            reader: None,
            child: launched.child,
        };
        let mut input = launched.stdin;
        crate::driver_input::set_nonblocking(&*input)
            .map_err(|error| failed(format!("Could not configure computer-use stdin: {error}")))?;
        let (sender, receiver) = crossbeam_channel::bounded::<Outbound>(OUTBOUND_QUEUE_CAPACITY);
        let writer_pending = pending.clone();
        let writer_connected = connected.clone();
        client.writer = Mutex::new(Some(
            thread::Builder::new()
                .name("computer-use-driver-writer".into())
                .spawn(move || {
                    loop {
                        let outbound = crossbeam_channel::select_biased! {
                            recv(stop_receiver) -> _ => break,
                            recv(receiver) -> outbound => match outbound {
                                Ok(outbound) => outbound,
                                Err(_) => break,
                            },
                        };
                        if !writer_connected.load(Ordering::SeqCst) {
                            break;
                        }
                        if outbound.started.elapsed() >= outbound.timeout {
                            continue;
                        }
                        let encoded = serde_json::to_vec(&RequestLine {
                            id: outbound.id,
                            request: outbound.request,
                        });
                        let mut bytes = match encoded {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                if let Some(sender) = writer_pending
                                    .lock()
                                    .expect("pending replies lock poisoned")
                                    .remove(&outbound.id)
                                {
                                    let _ = sender.send(Err(failed(format!(
                                        "Could not encode driver request: {error}"
                                    ))));
                                }
                                continue;
                            }
                        };
                        if outbound.started.elapsed() >= outbound.timeout {
                            continue;
                        }
                        bytes.push(b'\n');
                        if !write_request(&mut *input, &bytes, &writer_connected) {
                            disconnect(&writer_pending, &writer_connected);
                            break;
                        }
                    }
                })
                .map_err(|error| failed(format!("Could not start computer-use writer: {error}")))?,
        ));
        client.outgoing = Some(sender);
        client.reader = Some(
            thread::Builder::new()
                .name("computer-use-driver-reader".into())
                .spawn(move || {
                    // Also wake pending calls if the callback panics.
                    struct DisconnectOnExit(Pending, Arc<AtomicBool>);
                    impl Drop for DisconnectOnExit {
                        fn drop(&mut self) {
                            disconnect(&self.0, &self.1);
                        }
                    }
                    let _disconnect = DisconnectOnExit(pending.clone(), connected);
                    for line in BufReader::new(launched.stdout).lines() {
                        let Ok(line) = line else {
                            break;
                        };
                        let Ok(line) = serde_json::from_str::<DriverLine>(&line) else {
                            break;
                        };
                        match line {
                            DriverLine::Event(event) => on_event(event.event),
                            DriverLine::Response(response) => {
                                if let Some(sender) = pending
                                    .lock()
                                    .expect("pending replies lock poisoned")
                                    .remove(&response.id)
                                {
                                    let _ = sender.send(response.result);
                                }
                            }
                        }
                    }
                })
                .map_err(|error| failed(format!("Could not start computer-use reader: {error}")))?,
        );
        match client.call(
            Request::Hello {
                protocol: PROTOCOL_VERSION,
            },
            Duration::from_secs(30),
        ) {
            Ok(Reply::Hello {
                protocol,
                macos_major,
                host_bundle_id,
                permissions,
            }) => {
                if protocol != PROTOCOL_VERSION {
                    return Err(DriverError {
                        kind: ErrorKind::VersionMismatch,
                        message: "Orca was updated; start a new session to use computer use".into(),
                        candidates: vec![],
                    });
                }
                Ok((
                    client,
                    HelloInfo {
                        macos_major,
                        host_bundle_id,
                        permissions,
                    },
                ))
            }
            Ok(_) => Err(failed("Computer-use driver did not reply to hello")),
            Err(CallError::Driver(error)) => Err(error),
            Err(CallError::TimedOut) => Err(failed("Computer-use driver hello timed out")),
            Err(CallError::Disconnected) => {
                Err(failed("Computer-use driver disconnected during hello"))
            }
        }
    }

    /// Closes stdin and wakes every pending caller, even while another caller
    /// holds the client alive. Repeated closes are harmless.
    pub fn close(&self) {
        disconnect(&self.pending, &self.connected);
        // Wake an idle writer as well as one delivering a nonblocking frame.
        let _ = self.stop_writer.try_send(());
        let mut writer = self.writer.lock().expect("driver writer lock poisoned");
        if let Some(writer) = writer.take() {
            // The writer observes connected during pipe backpressure. Joining
            // ensures stdin is closed before returning, without waiting for a reply.
            let _ = writer.join();
        }
    }

    #[cfg(test)]
    fn queued_requests(&self) -> usize {
        self.outgoing.as_ref().map_or(0, Sender::len)
    }

    pub fn call(&self, request: Request, timeout: Duration) -> Result<Reply, CallError> {
        let started = Instant::now();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().expect("pending replies lock poisoned");
            if !self.connected.load(Ordering::SeqCst) {
                return Err(CallError::Disconnected);
            }
            pending.insert(id, sender);
        }
        let queued = self
            .outgoing
            .as_ref()
            .ok_or(CallError::Disconnected)?
            .send_timeout(
                Outbound {
                    id,
                    request,
                    started,
                    timeout,
                },
                timeout.saturating_sub(started.elapsed()),
            );
        match queued {
            Ok(()) => {}
            Err(SendTimeoutError::Timeout(_)) => {
                self.pending
                    .lock()
                    .expect("pending replies lock poisoned")
                    .remove(&id);
                return Err(CallError::TimedOut);
            }
            Err(SendTimeoutError::Disconnected(_)) => {
                disconnect(&self.pending, &self.connected);
                return Err(CallError::Disconnected);
            }
        }
        let result = match receiver.recv_timeout(timeout.saturating_sub(started.elapsed())) {
            Ok(result) => result.map_err(CallError::Driver),
            Err(RecvTimeoutError::Timeout) => Err(CallError::TimedOut),
            Err(RecvTimeoutError::Disconnected) => Err(CallError::Disconnected),
        };
        self.pending
            .lock()
            .expect("pending replies lock poisoned")
            .remove(&id);
        result
    }
}

impl Drop for DriverClient {
    fn drop(&mut self) {
        self.close();
        self.outgoing.take();
        if let Some(child) = &mut self.child {
            let deadline = Instant::now() + crate::timing::DRIVER_STOP_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        if let Some(reader) = self.reader.take()
            && reader.is_finished()
        {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeDesktop, FakeLauncher};
    use crate::protocol::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "condition did not become true in two seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn click(app: &str, element: u32) -> Request {
        Request::Act(ActParams {
            app: app.into(),
            window: None,
            action: Action::Click {
                target: Target::Element { element },
                button: Button::Left,
                count: 1,
                modifiers: vec![],
            },
            foreground_fallback: true,
            observe_after: false,
            include_screenshot: false,
            secure_approved: false,
        })
    }

    fn call(client: &DriverClient, request: Request) -> Result<Reply, CallError> {
        client.call(request, Duration::from_secs(5))
    }

    #[test]
    fn a_call_gets_its_reply_and_events_reach_the_callback() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let (client, hello) = DriverClient::start(
            &launcher,
            Arc::new(move |event| {
                assert_eq!(event, DriverEvent::Esc);
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        assert_eq!(hello.macos_major, 26);
        assert_eq!(hello.host_bundle_id.as_deref(), Some("com.cmuxterm.app"));
        assert!(
            hello.permissions.accessibility
                && hello.permissions.screen_recording
                && hello.permissions.event_tap
        );
        assert!(matches!(
            call(&client, Request::ListApps).unwrap(),
            Reply::Apps { .. }
        ));
        launcher.emit_esc();
        wait_until(|| seen.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn close_disconnects_every_pending_call_and_is_idempotent() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let client = Arc::new(client);
        launcher.desktop(|desktop| desktop.answer_after = Some(Duration::from_secs(5)));
        let (finished, results) = mpsc::channel();
        let calls: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                let finished = finished.clone();
                thread::spawn(move || {
                    let _ = finished.send(client.call(Request::ListApps, Duration::from_secs(60)));
                })
            })
            .collect();
        wait_until(|| client.pending.lock().unwrap().len() == 4);
        let (closed, closing) = mpsc::channel();
        let closing_client = client.clone();
        let closer = thread::spawn(move || {
            for _ in 0..3 {
                closing_client.close();
            }
            let _ = closed.send(());
        });
        for _ in 0..4 {
            let outcome = results.recv_timeout(Duration::from_millis(500));
            assert!(
                matches!(outcome, Ok(Err(CallError::Disconnected))),
                "pending call was not disconnected promptly: {outcome:?}"
            );
        }
        closing
            .recv_timeout(Duration::from_millis(500))
            .expect("close waited for the driver reply");
        assert!(matches!(
            call(&client, Request::ListApps),
            Err(CallError::Disconnected)
        ));
        for call in calls {
            call.join().unwrap();
        }
        closer.join().unwrap();
    }

    #[test]
    fn close_closes_idle_stdin_before_the_client_is_dropped() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let client = Arc::new(client);
        call(&client, Request::Turn { active: true }).unwrap();
        let (closed, closing) = mpsc::channel();
        let closing_client = client.clone();
        let closer = thread::spawn(move || {
            closing_client.close();
            let _ = closed.send(());
        });
        closing
            .recv_timeout(Duration::from_millis(500))
            .expect("close did not wake the idle writer");
        wait_until(|| launcher.shutdowns() == 1);
        assert!(!launcher.desktop(|desktop| desktop.turn_active));
        assert!(matches!(
            call(&client, Request::ListApps),
            Err(CallError::Disconnected)
        ));
        closer.join().unwrap();
    }

    #[test]
    fn a_slow_driver_times_out() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.answer_after = Some(Duration::from_millis(500));
        let launcher = FakeLauncher::new(desktop);
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        assert!(matches!(
            client.call(Request::ListApps, Duration::from_millis(50)),
            Err(CallError::TimedOut)
        ));
    }

    #[test]
    fn a_protocol_mismatch_is_reported_once_without_restarts() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.protocol = PROTOCOL_VERSION + 1;
        let launcher = FakeLauncher::new(desktop);
        let error = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap_err();
        assert_eq!(error.kind, ErrorKind::VersionMismatch);
        assert_eq!(
            error.message,
            "Orca was updated; start a new session to use computer use"
        );
        assert_eq!(launcher.launches(), 1);
        wait_until(|| launcher.shutdowns() == 1);
    }

    #[test]
    fn eof_on_stdin_runs_shutdown_before_exit() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        call(&client, Request::Turn { active: true }).unwrap();
        drop(client);
        wait_until(|| launcher.shutdowns() == 1);
        assert!(!launcher.desktop(|desktop| desktop.turn_active));
    }

    #[test]
    fn an_action_on_a_quit_app_fails_without_touching_another() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit_and_calculator());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        launcher.desktop(|desktop| desktop.quit_app("com.apple.TextEdit"));
        let error = call(&client, click("com.apple.TextEdit", 1)).unwrap_err();
        assert!(matches!(
            error,
            CallError::Driver(DriverError {
                kind: ErrorKind::AppNotFound,
                ..
            })
        ));
        assert_eq!(
            launcher.desktop(|desktop| desktop.actions_on("com.apple.calculator")),
            0
        );
    }

    #[test]
    fn shutdown_disconnects_current_and_future_calls() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        assert_eq!(call(&client, Request::Shutdown).unwrap(), Reply::Done);
        wait_until(|| launcher.shutdowns() == 1);
        assert!(matches!(
            call(&client, Request::ListApps),
            Err(CallError::Disconnected)
        ));
    }

    #[test]
    fn late_replies_do_not_get_delivered_to_the_next_call() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        launcher.desktop(|desktop| desktop.answer_after = Some(Duration::from_millis(100)));
        assert!(matches!(
            client.call(Request::ListApps, Duration::from_millis(10)),
            Err(CallError::TimedOut)
        ));
        assert_eq!(
            call(&client, Request::Turn { active: true }).unwrap(),
            Reply::Done
        );
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn concurrent_calls_receive_their_own_replies() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        thread::scope(|scope| {
            let apps = scope.spawn(|| call(&client, Request::ListApps).unwrap());
            let turn = scope.spawn(|| call(&client, Request::Turn { active: true }).unwrap());
            assert!(matches!(apps.join().unwrap(), Reply::Apps { .. }));
            assert_eq!(turn.join().unwrap(), Reply::Done);
        });
    }

    #[test]
    fn events_can_arrive_while_a_reply_is_delayed() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (sender, receiver) = mpsc::channel();
        let (client, _) = DriverClient::start(
            &launcher,
            Arc::new(move |event| {
                sender.send(event).unwrap();
            }),
        )
        .unwrap();
        launcher.desktop(|desktop| desktop.answer_after = Some(Duration::from_millis(200)));
        thread::scope(|scope| {
            let call = scope.spawn(|| call(&client, Request::ListApps).unwrap());
            launcher.emit_esc();
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
                DriverEvent::Esc
            );
            assert!(matches!(call.join().unwrap(), Reply::Apps { .. }));
        });
    }

    #[test]
    fn fake_reports_lock_conflicts_and_permissions_through_hello() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.lock_held = true;
        let launcher = FakeLauncher::new(desktop);
        assert_eq!(
            DriverClient::start(&launcher, Arc::new(|_| {}))
                .unwrap_err()
                .kind,
            ErrorKind::LockHeld
        );
        launcher.desktop(|desktop| {
            desktop.lock_held = false;
            desktop.permissions = Permissions::default();
        });
        let (_, hello) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        assert_eq!(hello.permissions, Permissions::default());
    }

    #[test]
    fn fake_resolution_preserves_ambiguity_and_launches_quit_apps() {
        let launcher = FakeLauncher::new(FakeDesktop::with_two_apps_named("Code"));
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let CallError::Driver(error) = call(
            &client,
            Request::ResolveApp {
                app: "Code".into(),
                allow_not_running: false,
            },
        )
        .unwrap_err() else {
            panic!("expected driver error")
        };
        assert_eq!(error.kind, ErrorKind::AppAmbiguous);
        assert_eq!(error.candidates.len(), 2);
        let id = "com.microsoft.VSCode";
        launcher.desktop(|desktop| desktop.quit_app(id));
        assert!(matches!(
            call(
                &client,
                Request::ResolveApp {
                    app: id.into(),
                    allow_not_running: false
                }
            ),
            Err(CallError::Driver(DriverError {
                kind: ErrorKind::AppNotFound,
                ..
            }))
        ));
        let Reply::Resolved { app } = call(
            &client,
            Request::ResolveApp {
                app: id.into(),
                allow_not_running: true,
            },
        )
        .unwrap() else {
            panic!("expected resolved")
        };
        assert_eq!(app.pid, None);
        let Reply::Launched { app } = call(
            &client,
            Request::LaunchApp {
                app: id.into(),
                open: vec![],
            },
        )
        .unwrap() else {
            panic!("expected launched")
        };
        assert!(app.pid.is_some());
    }

    #[test]
    fn fake_observations_filter_elements_and_actions_change_values() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let params = ObserveParams {
            app: "com.apple.TextEdit".into(),
            window: None,
            query: None,
            max_elements: 300,
            include_screenshot: true,
        };
        let Reply::Observed { observation } =
            call(&client, Request::Observe(params.clone())).unwrap()
        else {
            panic!("expected observation")
        };
        assert_eq!(observation.window.title, "probe.rtf");
        assert_eq!(
            observation
                .elements
                .iter()
                .map(|element| element.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 14]
        );
        assert!(observation.elements[2].secure);
        assert!(observation.elements[2].value.is_none());
        let screenshot = observation.screenshot.unwrap();
        use base64::Engine;
        let png = base64::engine::general_purpose::STANDARD
            .decode(&screenshot.png_base64)
            .unwrap();
        let image = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (image.width(), image.height()),
            (screenshot.width, screenshot.height)
        );
        assert!(image.width().max(image.height()) <= 1280);
        let mut action = match click("com.apple.TextEdit", 2) {
            Request::Act(params) => params,
            _ => unreachable!(),
        };
        action.action = Action::Type {
            text: "hello".into(),
            target: Some(Target::Element { element: 2 }),
        };
        action.observe_after = true;
        let Reply::Acted { outcome } = call(&client, Request::Act(action)).unwrap() else {
            panic!("expected action")
        };
        assert_eq!(
            outcome.observation.unwrap().elements[1].value.as_deref(),
            Some("hello")
        );
        assert_eq!(
            launcher.desktop(|desktop| desktop.actions_on("com.apple.TextEdit")),
            1
        );
        let filtered = ObserveParams {
            query: Some("Bold".into()),
            max_elements: 1,
            include_screenshot: false,
            ..params
        };
        let Reply::Observed { observation } =
            call(&client, Request::Observe(filtered.clone())).unwrap()
        else {
            panic!("expected filtered observation")
        };
        assert_eq!(observation.elements.len(), 1);
        assert_eq!(observation.elements[0].id, 1);
        assert_eq!(observation.omitted, 0);
        assert!(observation.screenshot.is_none());
        assert_eq!(
            launcher.desktop(|desktop| desktop.last_observe.clone()),
            Some(filtered)
        );
    }

    #[test]
    fn fake_fallback_and_secure_approval_errors_do_not_count_as_actions() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        launcher.desktop(|desktop| desktop.background_fails = true);
        let mut action = match click("com.apple.TextEdit", 1) {
            Request::Act(params) => params,
            _ => unreachable!(),
        };
        action.foreground_fallback = false;
        assert!(matches!(
            call(&client, Request::Act(action.clone())),
            Err(CallError::Driver(DriverError {
                kind: ErrorKind::NeedsForeground,
                ..
            }))
        ));
        action.foreground_fallback = true;
        launcher.desktop(|desktop| desktop.user_active = true);
        assert!(matches!(
            call(&client, Request::Act(action.clone())),
            Err(CallError::Driver(DriverError {
                kind: ErrorKind::UserActive,
                ..
            }))
        ));
        launcher.desktop(|desktop| desktop.user_active = false);
        let Reply::Acted { outcome } = call(&client, Request::Act(action.clone())).unwrap() else {
            panic!("expected action")
        };
        assert!(outcome.foreground_used);
        action.action = Action::Type {
            text: "secret".into(),
            target: Some(Target::Element { element: 14 }),
        };
        assert!(matches!(
            call(&client, Request::Act(action.clone())),
            Err(CallError::Driver(DriverError {
                kind: ErrorKind::NeedsSecureApproval,
                ..
            }))
        ));
        assert_eq!(
            launcher.desktop(|desktop| desktop.actions_on("com.apple.TextEdit")),
            1
        );
        action.secure_approved = true;
        action.observe_after = true;
        let Reply::Acted { outcome } = call(&client, Request::Act(action)).unwrap() else {
            panic!("expected action")
        };
        assert!(outcome.observation.unwrap().elements[2].value.is_none());
    }

    struct TrackedInput {
        pipe: io::PipeWriter,
        writing: Arc<AtomicBool>,
    }
    impl Write for TrackedInput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > 32 * 1024 {
                self.writing.store(true, Ordering::SeqCst);
            }
            self.pipe.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.pipe.flush()
        }
    }
    #[cfg(unix)]
    impl std::os::fd::AsFd for TrackedInput {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.pipe.as_fd()
        }
    }
    #[cfg(windows)]
    impl std::os::windows::io::AsHandle for TrackedInput {
        fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
            self.pipe.as_handle()
        }
    }
    struct StalledLauncher {
        input: Mutex<Option<io::PipeReader>>,
        output: Mutex<Option<crate::serve::LineWriter>>,
        writing: Arc<AtomicBool>,
    }
    impl DriverLauncher for StalledLauncher {
        fn launch(&self) -> io::Result<LaunchedDriver> {
            let (input, stdin) = io::pipe()?;
            let (stdout, output) = io::pipe()?;
            let output = crate::serve::LineWriter::new(output);
            output.send(&DriverLine::Response(ResponseLine {
                id: 1,
                result: Ok(Reply::Hello {
                    protocol: PROTOCOL_VERSION,
                    macos_major: 26,
                    host_bundle_id: None,
                    permissions: Permissions::default(),
                }),
            }))?;
            *self.input.lock().unwrap() = Some(input);
            *self.output.lock().unwrap() = Some(output);
            Ok(LaunchedDriver {
                stdin: Box::new(TrackedInput {
                    pipe: stdin,
                    writing: self.writing.clone(),
                }),
                stdout: Box::new(stdout),
                child: None,
            })
        }
    }

    #[test]
    fn a_full_stdin_pipe_times_out_both_calls_and_drop_closes_the_pipe() {
        let launcher = StalledLauncher {
            input: Mutex::new(None),
            output: Mutex::new(None),
            writing: Arc::new(AtomicBool::new(false)),
        };
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let client = Arc::new(client);
        let first_timeout = Duration::from_millis(300);
        let second_timeout = Duration::from_millis(100);
        let margin = Duration::from_millis(200);
        let payload = "x".repeat(4 * 1024 * 1024);
        let first_request = Request::LaunchApp {
            app: "com.apple.TextEdit".into(),
            open: vec![payload],
        };
        let (first, second) = thread::scope(|scope| {
            let (first_sender, first_receiver) = mpsc::channel();
            let (second_sender, second_receiver) = mpsc::channel();
            let first_client = client.clone();
            scope.spawn(move || {
                let started = Instant::now();
                let _ = first_sender.send((
                    first_client.call(first_request, first_timeout),
                    started.elapsed(),
                ));
            });
            wait_until(|| launcher.writing.load(Ordering::SeqCst));
            let second_client = client.clone();
            scope.spawn(move || {
                let started = Instant::now();
                let _ = second_sender.send((
                    second_client.call(Request::ListApps, second_timeout),
                    started.elapsed(),
                ));
            });
            let second = second_receiver.recv_timeout(second_timeout + margin);
            let first = first_receiver.recv_timeout(first_timeout + margin);
            // On RED, release the undrained pipe so the failed test cannot hang
            // while its scoped call threads are joined.
            if first.is_err() || second.is_err() {
                launcher.input.lock().unwrap().take();
            }
            (first, second)
        });
        let (first_result, first_elapsed) =
            first.expect("first call exceeded its deadline and margin");
        let (second_result, second_elapsed) =
            second.expect("second call waited behind a full pipe");
        assert!(matches!(first_result, Err(CallError::TimedOut)));
        assert!(matches!(second_result, Err(CallError::TimedOut)));
        assert!(first_elapsed <= first_timeout + margin);
        assert!(second_elapsed <= second_timeout + margin);
        let started = Instant::now();
        drop(client);
        assert!(
            started.elapsed() < margin,
            "drop waited for the stalled pipe reader"
        );
        let mut input = launcher.input.lock().unwrap().take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes).unwrap();
            sender.send(bytes.len()).unwrap();
        });
        let delivered = receiver
            .recv_timeout(margin)
            .expect("the writer kept stdin open after Drop");
        assert!(
            delivered < 4 * 1024 * 1024,
            "Drop allowed the blocked request to finish writing"
        );
    }

    #[test]
    fn close_interrupts_a_full_stdin_pipe_and_queued_call_admission() {
        let launcher = StalledLauncher {
            input: Mutex::new(None),
            output: Mutex::new(None),
            writing: Arc::new(AtomicBool::new(false)),
        };
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let client = Arc::new(client);
        let (finished, results) = mpsc::channel();
        let mut calls = Vec::new();
        let first_client = client.clone();
        let first_finished = finished.clone();
        calls.push(thread::spawn(move || {
            let _ = first_finished.send(first_client.call(
                Request::LaunchApp {
                    app: "com.apple.TextEdit".into(),
                    open: vec!["x".repeat(4 * 1024 * 1024)],
                },
                Duration::from_secs(60),
            ));
        }));
        wait_until(|| launcher.writing.load(Ordering::SeqCst));
        for _ in 0..12 {
            let client = client.clone();
            let finished = finished.clone();
            calls.push(thread::spawn(move || {
                let _ = finished.send(client.call(Request::ListApps, Duration::from_secs(60)));
            }));
        }
        wait_until(|| client.pending.lock().unwrap().len() == 13);
        let (closed, closing) = mpsc::channel();
        let closing_client = client.clone();
        let closer = thread::spawn(move || {
            closing_client.close();
            let _ = closed.send(());
        });
        closing
            .recv_timeout(Duration::from_millis(500))
            .expect("close waited for a full stdin pipe");
        for _ in 0..13 {
            let outcome = results.recv_timeout(Duration::from_millis(500));
            assert!(
                matches!(outcome, Ok(Err(CallError::Disconnected))),
                "close did not interrupt queued admission: {outcome:?}"
            );
        }
        let mut input = launcher.input.lock().unwrap().take().unwrap();
        let (eof, delivered) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes).unwrap();
            let _ = eof.send(bytes.len());
        });
        let delivered = delivered
            .recv_timeout(Duration::from_millis(500))
            .expect("close kept the stalled driver's stdin open");
        assert!(
            delivered < 4 * 1024 * 1024,
            "close allowed the blocked frame to finish"
        );
        reader.join().unwrap();
        for call in calls {
            call.join().unwrap();
        }
        closer.join().unwrap();
    }

    #[test]
    fn stalled_delivery_retains_only_a_bounded_queue_after_many_timeouts() {
        let launcher = StalledLauncher {
            input: Mutex::new(None),
            output: Mutex::new(None),
            writing: Arc::new(AtomicBool::new(false)),
        };
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        thread::scope(|scope| {
            let first = scope.spawn(|| {
                client.call(
                    Request::LaunchApp {
                        app: "com.apple.TextEdit".into(),
                        open: vec!["x".repeat(4 * 1024 * 1024)],
                    },
                    Duration::from_millis(300),
                )
            });
            wait_until(|| launcher.writing.load(Ordering::SeqCst));
            assert!(matches!(first.join().unwrap(), Err(CallError::TimedOut)));
        });
        let timeout = Duration::from_millis(20);
        let margin = Duration::from_millis(200);
        for _ in 0..50 {
            let request = Request::LaunchApp {
                app: "com.apple.TextEdit".into(),
                open: vec!["x".repeat(128 * 1024)],
            };
            let started = Instant::now();
            assert!(matches!(
                client.call(request, timeout),
                Err(CallError::TimedOut)
            ));
            assert!(
                started.elapsed() <= timeout + margin,
                "queue admission exceeded the call deadline and margin"
            );
        }
        let retained = client.queued_requests();
        assert!(client.pending.lock().unwrap().is_empty());
        assert!(
            retained <= OUTBOUND_QUEUE_CAPACITY,
            "stalled delivery retained {retained} queued payloads after their callers timed out; bound is {OUTBOUND_QUEUE_CAPACITY}"
        );
    }

    struct PipeLauncher(fn(BufReader<io::PipeReader>, crate::serve::LineWriter));
    impl DriverLauncher for PipeLauncher {
        fn launch(&self) -> io::Result<LaunchedDriver> {
            let (input, stdin) = io::pipe()?;
            let (stdout, output) = io::pipe()?;
            let run = self.0;
            thread::spawn(move || {
                let mut input = BufReader::new(input);
                let output = crate::serve::LineWriter::new(output);
                let hello = read_request(&mut input);
                assert_eq!(hello.id, 1);
                assert_eq!(
                    hello.request,
                    Request::Hello {
                        protocol: PROTOCOL_VERSION
                    }
                );
                let reply = Reply::Hello {
                    protocol: PROTOCOL_VERSION,
                    macos_major: 26,
                    host_bundle_id: None,
                    permissions: Permissions::default(),
                };
                output
                    .send(&DriverLine::Response(ResponseLine {
                        id: hello.id,
                        result: Ok(reply),
                    }))
                    .unwrap();
                run(input, output);
            });
            Ok(LaunchedDriver {
                stdin: Box::new(stdin),
                stdout: Box::new(stdout),
                child: None,
            })
        }
    }

    fn read_request(input: &mut impl BufRead) -> RequestLine {
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn responses_are_routed_by_id_even_when_the_driver_reverses_them() {
        let launcher = PipeLauncher(|mut input, output| {
            let first = read_request(&mut input);
            let second = read_request(&mut input);
            let mut ids = [first.id, second.id];
            ids.sort_unstable();
            assert_eq!(ids, [2, 3]);
            for request in [second, first] {
                let reply = match request.request {
                    Request::ListApps => Reply::Apps { apps: vec![] },
                    Request::Turn { .. } => Reply::Done,
                    _ => panic!("unexpected request"),
                };
                output
                    .send(&DriverLine::Response(ResponseLine {
                        id: request.id,
                        result: Ok(reply),
                    }))
                    .unwrap();
            }
        });
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        thread::scope(|scope| {
            let apps = scope.spawn(|| call(&client, Request::ListApps).unwrap());
            let turn = scope.spawn(|| call(&client, Request::Turn { active: true }).unwrap());
            assert!(matches!(apps.join().unwrap(), Reply::Apps { .. }));
            assert_eq!(turn.join().unwrap(), Reply::Done);
        });
    }

    #[test]
    fn stdout_eof_wakes_all_waiting_calls_and_rejects_new_calls() {
        let launcher = PipeLauncher(|mut input, _output| {
            read_request(&mut input);
            read_request(&mut input);
        });
        let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        thread::scope(|scope| {
            let first = scope.spawn(|| call(&client, Request::ListApps));
            let second = scope.spawn(|| call(&client, Request::Turn { active: true }));
            assert!(matches!(
                first.join().unwrap(),
                Err(CallError::Disconnected)
            ));
            assert!(matches!(
                second.join().unwrap(),
                Err(CallError::Disconnected)
            ));
        });
        assert!(matches!(
            call(&client, Request::ListApps),
            Err(CallError::Disconnected)
        ));
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn drop_reaps_a_child_that_exits_on_eof_and_kills_one_that_ignores_it() {
        use std::process::{Command, Stdio};
        struct ProcessLauncher {
            ignore_eof: bool,
            pid: AtomicUsize,
        }
        impl DriverLauncher for ProcessLauncher {
            fn launch(&self) -> io::Result<LaunchedDriver> {
                let hello = serde_json::to_string(&DriverLine::Response(ResponseLine {
                    id: 1,
                    result: Ok(Reply::Hello {
                        protocol: PROTOCOL_VERSION,
                        macos_major: 26,
                        host_bundle_id: None,
                        permissions: Permissions::default(),
                    }),
                }))?;
                let end = if self.ignore_eof {
                    "exec sleep 120"
                } else {
                    "while IFS= read -r line; do :; done; exit 7"
                };
                let script = format!("IFS= read -r line; printf '%s\\n' '{hello}'; {end}");
                let mut child = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(script)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                self.pid.store(child.id() as usize, Ordering::SeqCst);
                Ok(LaunchedDriver {
                    stdin: Box::new(child.stdin.take().unwrap()),
                    stdout: Box::new(child.stdout.take().unwrap()),
                    child: Some(child),
                })
            }
        }
        for ignore_eof in [false, true] {
            let launcher = ProcessLauncher {
                ignore_eof,
                pid: AtomicUsize::new(0),
            };
            let (client, _) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
            let started = Instant::now();
            drop(client);
            assert!(started.elapsed() < Duration::from_secs(40));
            if ignore_eof {
                assert!(started.elapsed() >= Duration::from_secs(37));
            }
            let pid = launcher.pid.load(Ordering::SeqCst).to_string();
            assert!(
                !Command::new("/bin/kill")
                    .args(["-0", &pid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                "child still exists after drop"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn stop_allows_a_delayed_eof_cleanup_to_finish_before_killing() {
        use std::process::{Command, Stdio};
        struct CleanupLauncher {
            marker: std::path::PathBuf,
        }
        impl DriverLauncher for CleanupLauncher {
            fn launch(&self) -> io::Result<LaunchedDriver> {
                let hello = serde_json::to_string(&DriverLine::Response(ResponseLine {
                    id: 1,
                    result: Ok(Reply::Hello {
                        protocol: PROTOCOL_VERSION,
                        macos_major: 26,
                        host_bundle_id: None,
                        permissions: Permissions::default(),
                    }),
                }))?;
                let script = format!(
                    "IFS= read -r line; printf '%s\\n' '{hello}'; while IFS= read -r line; do :; done; sleep 2; printf cleanup > \"$1\""
                );
                let mut child = Command::new("/bin/sh")
                    .args(["-c", &script, "cleanup-driver"])
                    .arg(&self.marker)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                Ok(LaunchedDriver {
                    stdin: Box::new(child.stdin.take().unwrap()),
                    stdout: Box::new(child.stdout.take().unwrap()),
                    child: Some(child),
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("restored");
        let (client, _) = DriverClient::start(
            &CleanupLauncher {
                marker: marker.clone(),
            },
            Arc::new(|_| {}),
        )
        .unwrap();
        drop(client);
        assert_eq!(
            std::fs::read_to_string(marker).ok().as_deref(),
            Some("cleanup"),
            "driver was killed before its EOF restoration completed"
        );
    }
}
