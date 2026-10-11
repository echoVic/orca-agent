use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_computer_use::client::{
    CallError, DriverClient, DriverLauncher, HelloInfo, LaunchedDriver,
};
use orca_computer_use::protocol::{DriverError, ErrorKind, Observation, Reply, Request};
use orca_core::approval_types::ApprovalMode;
use orca_core::config::RunConfig;

use crate::extension::ExtensionData;

pub(crate) mod prepare;

pub(crate) struct ComputerUseService {
    launcher: Arc<dyn DriverLauncher>,
    state: Mutex<ServiceState>,
}

#[derive(Default)]
struct ServiceState {
    driver: Option<(Arc<DriverClient>, HelloInfo)>,
    generation: u64,
    version_mismatch: Option<DriverError>,
    observations: HashMap<(String, u32), SecureObservation>,
    latest_window: HashMap<String, u32>,
}

struct SecureObservation {
    elements: HashSet<u32>,
    focused: bool,
}

impl ServiceState {
    fn start(&mut self, launcher: &dyn DriverLauncher) -> Result<(), DriverError> {
        if let Some(error) = &self.version_mismatch {
            return Err(error.clone());
        }
        if self.driver.is_none() {
            // Task 10 attaches the turn's Esc handling to driver events.
            match DriverClient::start(launcher, Arc::new(|_| {})) {
                Ok((client, hello)) => {
                    self.generation = self
                        .generation
                        .checked_add(1)
                        .expect("driver generation overflow");
                    self.driver = Some((Arc::new(client), hello));
                }
                Err(error) => {
                    if error.kind == ErrorKind::VersionMismatch {
                        self.version_mismatch = Some(error.clone());
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        if let Some((client, _)) = &self.driver {
            client.close();
        }
        self.driver = None;
        self.observations.clear();
        self.latest_window.clear();
    }

    fn observation(&self, bundle_id: &str, window: Option<u32>) -> Option<&SecureObservation> {
        let window = window.or_else(|| self.latest_window.get(bundle_id).copied())?;
        self.observations.get(&(bundle_id.to_owned(), window))
    }
}

impl ComputerUseService {
    pub(crate) fn new(launcher: Arc<dyn DriverLauncher>) -> Self {
        Self {
            launcher,
            state: Mutex::new(ServiceState::default()),
        }
    }

    /// Starts on first use; a version mismatch remains failed for this session.
    pub(crate) fn hello(&self) -> Result<HelloInfo, DriverError> {
        let mut state = self
            .state
            .lock()
            .expect("computer-use service lock poisoned");
        state.start(&*self.launcher)?;
        Ok(state.driver.as_ref().expect("started driver").1.clone())
    }

    pub(crate) fn call(&self, request: Request, timeout: Duration) -> Result<Reply, CallError> {
        let (client, generation) = {
            let mut state = self
                .state
                .lock()
                .expect("computer-use service lock poisoned");
            state.start(&*self.launcher).map_err(CallError::Driver)?;
            (
                Arc::clone(&state.driver.as_ref().expect("started driver").0),
                state.generation,
            )
        };
        let result = client.call(request, timeout);
        if matches!(result, Err(CallError::TimedOut | CallError::Disconnected)) {
            let mut state = self
                .state
                .lock()
                .expect("computer-use service lock poisoned");
            if state.generation == generation {
                state.reset();
            }
        }
        result
    }

    /// Disconnects the active call without waiting for a driver reply.
    #[allow(dead_code)] // Used by Task 10's Esc path.
    pub(crate) fn cancel_in_flight(&self) {
        let client = self
            .state
            .lock()
            .expect("computer-use service lock poisoned")
            .driver
            .as_ref()
            .map(|(client, _)| Arc::clone(client));
        if let Some(client) = client {
            client.close();
        }
    }

    /// Drops the client and its observations; the next call starts a new driver.
    #[allow(dead_code)] // Used by Task 10 tool cancellation.
    pub(crate) fn reset(&self) {
        self.state
            .lock()
            .expect("computer-use service lock poisoned")
            .reset();
    }

    #[allow(dead_code)] // Used by Task 10 observation results.
    pub(crate) fn record_observation(&self, observation: &Observation) {
        let mut state = self
            .state
            .lock()
            .expect("computer-use service lock poisoned");
        state
            .latest_window
            .insert(observation.bundle_id.clone(), observation.window.id);
        state.observations.insert(
            (observation.bundle_id.clone(), observation.window.id),
            SecureObservation {
                elements: observation
                    .elements
                    .iter()
                    .filter(|element| element.secure)
                    .map(|element| element.id)
                    .collect(),
                focused: observation.focused_secure,
            },
        );
    }

    pub(crate) fn is_secure(&self, bundle_id: &str, window: Option<u32>, id: u32) -> bool {
        self.state
            .lock()
            .expect("computer-use service lock poisoned")
            .observation(bundle_id, window)
            .is_some_and(|observation| observation.elements.contains(&id))
    }

    pub(crate) fn focused_secure(&self, bundle_id: &str, window: Option<u32>) -> bool {
        self.state
            .lock()
            .expect("computer-use service lock poisoned")
            .observation(bundle_id, window)
            .is_some_and(|observation| observation.focused)
    }
}

/// The thread's service; constructing it does not start the driver.
pub(crate) fn service(
    thread_store: &ExtensionData,
    cwd: &Path,
    config: &RunConfig,
) -> Arc<ComputerUseService> {
    thread_store.get_or_init(|| {
        ComputerUseService::new(Arc::new(ProcessLauncher {
            exe: std::env::current_exe().expect("current executable path"),
            cwd: cwd.to_path_buf(),
            approval_mode: config.approval_mode,
        }))
    })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct ProcessLauncher {
    exe: PathBuf,
    cwd: PathBuf,
    approval_mode: ApprovalMode,
}

impl DriverLauncher for ProcessLauncher {
    #[cfg(target_os = "macos")]
    fn launch(&self) -> io::Result<LaunchedDriver> {
        use crate::execution_broker::{ExecutionBroker, LaunchError};
        use orca_core::capability::{CapabilitySet, EnforcementState};
        use std::process::{Command, Stdio};

        let home = orca_core::home::orca_home()
            .ok_or_else(|| io::Error::other("Could not locate ORCA_HOME"))?;
        let logs = home.join("logs");
        std::fs::create_dir_all(&logs)?;
        let log = std::fs::File::create(logs.join("computer-use-driver.log"))?;
        let mut command = Command::new(&self.exe);
        command
            .arg("computer-use-driver")
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log));
        let launched =
            ExecutionBroker::with_backend(EnforcementState::Advisory, "computer-use-driver")
                .launch_user_trusted(
                    command,
                    "computer-use-driver",
                    &self.cwd,
                    CapabilitySet::for_approval_mode(self.approval_mode),
                )
                .map_err(|error| match error {
                    LaunchError::Spawn(error) => error,
                    other => io::Error::other(format!("{other:?}")),
                })?;
        let mut child = launched.child;
        let stdin = child.stdin.take().expect("piped driver stdin");
        let stdout = child.stdout.take().expect("piped driver stdout");
        Ok(LaunchedDriver {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            child: Some(child),
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn launch(&self) -> io::Result<LaunchedDriver> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "computer use is only available in the interactive TUI on macOS 14 or later",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::ExtensionData;
    use orca_computer_use::client::CallError;
    use orca_computer_use::fake::{FakeDesktop, FakeLauncher};
    use orca_computer_use::protocol::{ErrorKind, ObserveParams, Reply, Request};
    use orca_core::config::RunConfig;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn the_service_starts_lazily_and_reuses_the_handshake() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = ComputerUseService::new(Arc::new(launcher.clone()));
        assert_eq!(launcher.launches(), 0);
        assert_eq!(
            service.hello().unwrap().host_bundle_id.as_deref(),
            Some("com.cmuxterm.app")
        );
        service.hello().unwrap();
        assert!(matches!(
            service
                .call(Request::ListApps, Duration::from_secs(30))
                .unwrap(),
            Reply::Apps { .. }
        ));
        assert_eq!(launcher.launches(), 1);
        service.reset();
        service.hello().unwrap();
        assert_eq!(launcher.launches(), 2);
    }

    #[test]
    fn a_version_mismatch_stays_failed_even_after_reset() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.protocol = 999;
        let launcher = FakeLauncher::new(desktop);
        let service = ComputerUseService::new(Arc::new(launcher.clone()));
        let error = service.hello().unwrap_err();
        assert_eq!(error.kind, ErrorKind::VersionMismatch);
        launcher.desktop(|desktop| desktop.protocol = 1);
        service.reset();
        assert_eq!(service.hello().unwrap_err(), error);
        assert!(
            matches!(service.call(Request::ListApps, Duration::from_secs(30)), Err(CallError::Driver(error)) if error.kind == ErrorKind::VersionMismatch)
        );
        assert_eq!(launcher.launches(), 1);
    }

    #[test]
    fn a_temporary_hello_failure_can_be_retried() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.lock_held = true;
        let launcher = FakeLauncher::new(desktop);
        let service = ComputerUseService::new(Arc::new(launcher.clone()));
        assert_eq!(service.hello().unwrap_err().kind, ErrorKind::LockHeld);
        launcher.desktop(|desktop| desktop.lock_held = false);
        service.hello().unwrap();
        assert_eq!(launcher.launches(), 2);
    }

    fn assert_interrupt_disconnects_a_slow_call(interrupt: fn(&ComputerUseService)) {
        use std::sync::mpsc;
        use std::thread;

        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = Arc::new(ComputerUseService::new(Arc::new(launcher.clone())));
        service.hello().unwrap();
        let (started, received) = mpsc::channel();
        launcher.desktop(|desktop| {
            desktop.request_started = Some(started);
            desktop.answer_after = Some(Duration::from_secs(5));
        });
        let (finished, result) = mpsc::channel();
        let calling_service = service.clone();
        let call = thread::spawn(move || {
            let _ = finished.send(calling_service.call(
                Request::LaunchApp {
                    app: "com.apple.TextEdit".into(),
                    open: vec![],
                },
                Duration::from_secs(60),
            ));
        });
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Request::LaunchApp { .. }
        ));
        let (reset_done, reset_result) = mpsc::channel();
        let resetting_service = service.clone();
        let reset = thread::spawn(move || {
            interrupt(&resetting_service);
            let _ = reset_done.send(());
        });
        let outcome = result.recv_timeout(Duration::from_millis(500));
        assert!(
            matches!(outcome, Ok(Err(CallError::Disconnected))),
            "interrupt did not disconnect the pending call promptly: {outcome:?}"
        );
        reset_result
            .recv_timeout(Duration::from_millis(500))
            .expect("interrupt waited for the slow driver");
        call.join().unwrap();
        reset.join().unwrap();
    }

    #[test]
    fn reset_disconnects_a_slow_call_without_waiting_for_its_reply() {
        assert_interrupt_disconnects_a_slow_call(ComputerUseService::reset);
    }

    #[test]
    fn cancel_in_flight_disconnects_a_slow_call_without_waiting_for_its_reply() {
        assert_interrupt_disconnects_a_slow_call(ComputerUseService::cancel_in_flight);
    }

    #[test]
    fn an_old_disconnected_call_cannot_reset_a_newer_driver() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        use std::thread;

        struct ReplacingLauncher {
            first: FakeLauncher,
            replacement: FakeLauncher,
            launches: AtomicUsize,
        }
        impl DriverLauncher for ReplacingLauncher {
            fn launch(&self) -> io::Result<LaunchedDriver> {
                if self.launches.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.first.launch()
                } else {
                    self.replacement.launch()
                }
            }
        }
        let first = FakeLauncher::new(FakeDesktop::with_textedit());
        let launcher = Arc::new(ReplacingLauncher {
            first: first.clone(),
            replacement: FakeLauncher::new(FakeDesktop::with_textedit()),
            launches: AtomicUsize::new(0),
        });
        let service = Arc::new(ComputerUseService::new(launcher.clone()));
        service.hello().unwrap();
        let (started, received) = mpsc::channel();
        first.desktop(|desktop| {
            desktop.request_started = Some(started);
            desktop.answer_after = Some(Duration::from_secs(5));
        });
        let (finished, result) = mpsc::channel();
        let calling_service = service.clone();
        let call = thread::spawn(move || {
            let _ = finished.send(calling_service.call(Request::ListApps, Duration::from_secs(60)));
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Request::ListApps
        );
        {
            // Hold the normal cleanup lock across replacement: the failed old
            // request cannot finish its cleanup before the new driver exists.
            let mut state = service.state.lock().unwrap();
            state.reset();
            state.start(&*service.launcher).unwrap();
        }
        let outcome = result.recv_timeout(Duration::from_millis(500));
        assert!(
            matches!(outcome, Ok(Err(CallError::Disconnected))),
            "old call did not disconnect: {outcome:?}"
        );
        call.join().unwrap();
        assert!(matches!(
            service
                .call(Request::ListApps, Duration::from_secs(30))
                .unwrap(),
            Reply::Apps { .. }
        ));
        assert_eq!(
            launcher.launches.load(Ordering::SeqCst),
            2,
            "the old call reset the replacement driver"
        );
    }

    #[test]
    fn a_timed_out_call_drops_the_driver_before_the_next_call() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = ComputerUseService::new(Arc::new(launcher.clone()));
        service.hello().unwrap();
        launcher.desktop(|desktop| desktop.answer_after = Some(Duration::from_millis(100)));
        assert!(matches!(
            service.call(Request::ListApps, Duration::from_millis(5)),
            Err(CallError::TimedOut)
        ));
        launcher.desktop(|desktop| desktop.answer_after = None);
        service
            .call(Request::ListApps, Duration::from_secs(30))
            .unwrap();
        assert_eq!(launcher.launches(), 2);
    }

    #[test]
    fn secure_metadata_tracks_the_latest_observation_per_app_and_window() {
        let service =
            ComputerUseService::new(Arc::new(FakeLauncher::new(FakeDesktop::with_textedit())));
        let Reply::Observed { mut observation } = service
            .call(
                Request::Observe(ObserveParams {
                    app: "com.apple.TextEdit".into(),
                    window: None,
                    query: None,
                    max_elements: 300,
                    include_screenshot: false,
                }),
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("observation")
        };
        observation.focused_secure = true;
        let secure_observation = observation.clone();
        service.record_observation(&observation);
        assert!(service.is_secure("com.apple.TextEdit", None, 14));
        assert!(service.focused_secure("com.apple.TextEdit", Some(1)));
        assert!(!service.is_secure("com.apple.calculator", Some(1), 14));
        assert!(!service.is_secure("com.apple.TextEdit", Some(2), 14));
        observation.window.id = 2;
        observation.focused_secure = false;
        observation.elements.clear();
        service.record_observation(&observation);
        assert!(!service.is_secure("com.apple.TextEdit", None, 14));
        assert!(service.is_secure("com.apple.TextEdit", Some(1), 14));
        assert!(!service.focused_secure("com.apple.TextEdit", None));
        observation.window.id = 1;
        service.record_observation(&observation);
        assert!(!service.is_secure("com.apple.TextEdit", Some(1), 14));
        service.record_observation(&secure_observation);
        assert!(service.is_secure("com.apple.TextEdit", None, 14));
        assert!(service.focused_secure("com.apple.TextEdit", Some(1)));
        service.reset();
        assert!(!service.is_secure("com.apple.TextEdit", None, 14));
        assert!(!service.focused_secure("com.apple.TextEdit", Some(1)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_launcher_pipes_stdio_and_truncates_the_isolated_driver_log() {
        use orca_computer_use::client::DriverLauncher;
        use orca_core::approval_types::ApprovalMode;
        use std::io::{Read, Write};

        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(
            cwd.path().join("computer-use-driver"),
            r#"printf '%s\n' "$0"
printf '%s\n' "$#"
printf '%s\n' "$PWD"
printf 'driver stderr\n' >&2
IFS= read -r line
printf '%s\n' "$line"
"#,
        )
        .unwrap();
        // /bin/sh takes the driver's subcommand name as this fixture's script.
        // The production launcher still supplies precisely one argument.
        let log = home.path().join("logs/computer-use-driver.log");
        let launcher = ProcessLauncher {
            exe: "/bin/sh".into(),
            cwd: cwd.path().to_path_buf(),
            approval_mode: ApprovalMode::Suggest,
        };
        for old_log in [None, Some("old log content")] {
            if let Some(old_log) = old_log {
                std::fs::write(&log, old_log).unwrap();
            }
            let mut launched = launcher.launch().unwrap();
            launched.stdin.write_all(b"pipe probe\n").unwrap();
            drop(launched.stdin);
            let mut stdout = String::new();
            launched.stdout.read_to_string(&mut stdout).unwrap();
            assert!(launched.child.as_mut().unwrap().wait().unwrap().success());
            let lines: Vec<_> = stdout.lines().collect();
            assert_eq!(lines.len(), 4);
            assert_eq!(lines[0], "computer-use-driver");
            assert_eq!(lines[1], "0", "no additional driver arguments");
            assert_eq!(
                std::path::Path::new(lines[2]).canonicalize().unwrap(),
                cwd.path().canonicalize().unwrap()
            );
            assert_eq!(lines[3], "pipe probe");
            assert_eq!(std::fs::read_to_string(&log).unwrap(), "driver stderr\n");
        }
    }

    #[test]
    fn each_thread_owns_one_service() {
        let first = ExtensionData::new("first");
        let second = ExtensionData::new("second");
        let config = RunConfig::default();
        let cwd = std::path::Path::new(".");
        let one = service(&first, cwd, &config);
        assert!(Arc::ptr_eq(&one, &service(&first, cwd, &config)));
        assert!(!Arc::ptr_eq(&one, &service(&second, cwd, &config)));
    }
}
