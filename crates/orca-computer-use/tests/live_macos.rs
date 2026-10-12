#![cfg(target_os = "macos")]
#![cfg_attr(target_os = "macos", no_main)]
#![cfg_attr(target_os = "macos", reexport_test_harness_main = "rust_tests")]

// Keep the standard ignored/serial libtest interface, but let AppKit's actual
// main thread deliver runningApplications updates while libtest runs tests.
#[unsafe(export_name = "main")]
extern "C" fn macos_test_main() -> i32 {
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    let workspace = NSWorkspace::sharedWorkspace();
    let frontmost = workspace
        .frontmostApplication()
        .map(|app| app.processIdentifier());
    let pointer = CGEvent::location(Some(&CGEvent::new(None).expect("read suite pointer")));
    let harness = std::thread::spawn(rust_tests);
    while !harness.is_finished() {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, true);
    }
    let passed = harness.join().is_ok();
    let restored = restore_suite_if_live(
        LIVE_TEST_RAN.load(std::sync::atomic::Ordering::SeqCst),
        || run_live_test(|| record_restoration(frontmost, pointer)),
    );
    let failures = RESTORATION_FAILURES.lock().unwrap();
    for failure in failures.iter() {
        eprintln!("GUI restoration failed: {failure}");
    }
    if passed && restored && failures.is_empty() {
        0
    } else {
        101
    }
}

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

static RESTORATION_FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());
// GUI tests are serial even when libtest/nextest requests concurrency. Every
// retry recreates all locals, so Live records and restores a fresh starting state.
static LIVE_TEST_RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LIVE_TEST_LOCK: Mutex<()> = Mutex::new(());
thread_local! {
    static ATTEMPT: std::cell::RefCell<Option<LiveAttempt>> = const { std::cell::RefCell::new(None) };
}

struct LiveAttempt {
    started: Instant,
    idle_at_start: f64,
    last_own_hid: Option<f64>,
    inconclusive: bool,
    failures: Vec<String>,
}

fn hid_idle_seconds() -> f64 {
    CGEventSource::seconds_since_last_event_type(
        CGEventSourceStateID::HIDSystemState,
        CGEventType(u32::MAX), // kCGAnyInputEventType
    )
}

// Task 8's helpers must call this at each marked HID post, rather than at the
// end of a batch: user input after the last test event must remain detectable.
#[allow(dead_code)]
fn record_test_hid_event() {
    ATTEMPT.with(|active| {
        if let Some(attempt) = active.borrow_mut().as_mut() {
            attempt.last_own_hid = Some(attempt.started.elapsed().as_secs_f64());
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum GuiFailureKind {
    StateMismatch,
    NativeOperation,
}

fn driver_hid_since(started: Instant) -> Option<f64> {
    orca_computer_use::driver::macos::last_test_process_hid_post()
        .and_then(|posted| posted.checked_duration_since(started))
        .map(|duration| duration.as_secs_f64())
}

fn record_gui_failure(kind: GuiFailureKind, message: String) {
    ATTEMPT.with(|active| {
        let mut active = active.borrow_mut();
        if let Some(attempt) = active.as_mut() {
            let elapsed = attempt.started.elapsed().as_secs_f64();
            let idle = hid_idle_seconds();
            if failure_is_inconclusive(kind, elapsed, idle, attempt.last_own_hid, driver_hid_since(attempt.started)) {
                attempt.inconclusive = true;
                eprintln!("inconclusive: {message}; HID idle={idle:.3}s, elapsed={elapsed:.3}s, starting idle={:.3}s", attempt.idle_at_start);
            } else {
                attempt.failures.push(message);
            }
        } else {
            RESTORATION_FAILURES.lock().unwrap().push(message);
        }
    });
}

fn run_live_test(body: impl Fn()) {
    let _serial = LIVE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for number in 1..=3 {
        let started = Instant::now();
        let idle_at_start = hid_idle_seconds();
        ATTEMPT.with(|active| {
            *active.borrow_mut() = Some(LiveAttempt {
                started,
                idle_at_start,
                last_own_hid: None,
                inconclusive: false,
                failures: vec![],
            })
        });
        let failures_before = RESTORATION_FAILURES.lock().unwrap().len();
        // Cleanup runs on both normal return and panic. Only observed state
        // mismatches can be inconclusive; native errors and action assertions
        // always fail normally, even if the same attempt has user activity.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&body));
        let attempt = ATTEMPT.with(|active| active.borrow_mut().take().unwrap());
        if let Err(failure) = result {
            std::panic::resume_unwind(failure);
        }
        assert!(
            attempt.failures.is_empty(),
            "GUI state/restore failed: {:?}",
            attempt.failures
        );
        assert_eq!(
            RESTORATION_FAILURES.lock().unwrap().len(),
            failures_before,
            "test resource cleanup failed"
        );
        if !attempt.inconclusive {
            return;
        }
        if number < 3 {
            eprintln!("rerunning live test after user activity (attempt {number}/3)");
        }
    }
    eprintln!("skipped: user activity during every attempt");
}

use std::thread;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use objc2_core_graphics::{
    CGEvent, CGEventSource, CGEventSourceStateID, CGEventType, CGWarpMouseCursorPosition,
};
use objc2_foundation::NSString;
use orca_computer_use::client::{DriverClient, HelloInfo};
use orca_computer_use::driver::macos::MacDriver;
use orca_computer_use::inprocess::InProcessLauncher;
use orca_computer_use::protocol::{
    ActOutcome, ActParams, Action, AppInfo, Button, Direction, DriverError, ErrorKind, Observation,
    ObserveParams, Reply, Request, Target,
};

fn fixture_app() -> PathBuf {
    let app = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("CUFixture.app");
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::write(app.join("Contents/Info.plist"), r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.orca.cu-fixture</string>
<key>CFBundleName</key><string>CUFixture</string>
<key>CFBundleExecutable</key><string>CUFixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSPrincipalClass</key><string>CUProbeApplication</string>
</dict></plist>"#).unwrap();
    let module_cache = tempfile::tempdir_in("/tmp").unwrap();
    let output = Command::new("/usr/bin/swiftc")
        .arg("-O")
        .arg("-module-cache-path")
        .arg(module_cache.path())
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/CUFixture.swift"
        ))
        .arg("-o")
        .arg(macos.join("CUFixture"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "swiftc: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    app
}

struct Live {
    client: DriverClient,
    hello: HelloInfo,
    directory: tempfile::TempDir,
    frontmost: Option<i32>,
    pointer: objc2_core_foundation::CGPoint,
    running: HashSet<i32>,
    launched: HashSet<i32>,
    background_state: Option<(i32, objc2_core_foundation::CGPoint)>,
    esc_events: Arc<std::sync::atomic::AtomicUsize>,
    restore_state: bool,
}

impl Live {
    fn start() -> Option<Self> {
        if !std::path::Path::new("/usr/bin/swiftc").exists() {
            eprintln!("skipped: swiftc not found");
            return None;
        }
        let workspace = NSWorkspace::new();
        let frontmost = workspace
            .frontmostApplication()
            .map(|app| app.processIdentifier());
        let running: HashSet<i32> = workspace
            .runningApplications()
            .iter()
            .map(|app| app.processIdentifier())
            .collect();
        let pointer = CGEvent::location(Some(&CGEvent::new(None).expect("read pointer position")));
        eprintln!(
            "live starting state: frontmost={frontmost:?}, pointer={pointer:?}, running={running:?}"
        );
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let launcher =
            InProcessLauncher::new(MacDriver::new(directory.path().join("computer-use.lock")));
        let esc_events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received = esc_events.clone();
        let (client, hello) = DriverClient::start(
            &launcher,
            Arc::new(move |_| {
                received.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        )
        .unwrap();
        let mut live = Self {
            client,
            hello,
            directory,
            frontmost,
            pointer,
            running,
            launched: HashSet::new(),
            background_state: None,
            esc_events,
            restore_state: false,
        };
        let permissions = live.hello.permissions;
        if !permissions.accessibility || !permissions.screen_recording || !permissions.event_tap {
            eprintln!("skipped: missing permission: {permissions:?}");
            return None;
        }
        LIVE_TEST_RAN.store(true, std::sync::atomic::Ordering::SeqCst);
        live.restore_state = true;
        Some(live)
    }

    fn apps(&self) -> Vec<AppInfo> {
        match self
            .client
            .call(Request::ListApps, Duration::from_secs(30))
            .unwrap()
        {
            Reply::Apps { apps } => apps,
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    fn observe(&self, app: &str, query: Option<&str>) -> Observation {
        self.observe_window(app, None, query, 300, true)
    }

    fn observe_window(
        &self,
        app: &str,
        window: Option<u32>,
        query: Option<&str>,
        max_elements: u32,
        include_screenshot: bool,
    ) -> Observation {
        match self
            .client
            .call(
                Request::Observe(ObserveParams {
                    app: app.into(),
                    window,
                    query: query.map(str::to_owned),
                    max_elements,
                    include_screenshot,
                }),
                Duration::from_secs(30),
            )
            .unwrap()
        {
            Reply::Observed { observation } => observation,
            other => panic!("unexpected observe reply: {other:?}"),
        }
    }

    fn act(&self, action: Action, secure_approved: bool) -> Result<ActOutcome, DriverError> {
        self.act_with_fallback(action, secure_approved, false)
    }

    fn act_with_fallback(
        &self,
        action: Action,
        secure_approved: bool,
        foreground_fallback: bool,
    ) -> Result<ActOutcome, DriverError> {
        self.act_app(
            "dev.orca.cu-fixture",
            None,
            action,
            secure_approved,
            foreground_fallback,
        )
    }

    fn act_app(
        &self,
        app: &str,
        window: Option<u32>,
        action: Action,
        secure_approved: bool,
        foreground_fallback: bool,
    ) -> Result<ActOutcome, DriverError> {
        match self
            .client
            .call(
                Request::Act(ActParams {
                    app: app.into(),
                    window,
                    action,
                    foreground_fallback,
                    observe_after: true,
                    include_screenshot: false,
                    secure_approved,
                }),
                Duration::from_secs(30),
            )
            .map_err(|e| match e {
                orca_computer_use::client::CallError::Driver(e) => e,
                other => panic!("driver transport: {other:?}"),
            })? {
            Reply::Acted { outcome } => Ok(outcome),
            other => panic!("unexpected act reply: {other:?}"),
        }
    }

    fn point_inside(&self, observation: &Observation, name: &str) -> Target {
        if name == "canvas" {
            let image = observation.screenshot.as_ref().unwrap();
            return Target::Point {
                point: [f64::from(image.width) * 0.75, f64::from(image.height) * 0.5],
            };
        }
        let frame = observation
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some(name))
            .unwrap()
            .frame;
        Target::Point {
            point: [
                f64::from(frame[0]) + f64::from(frame[2]) / 2.0,
                f64::from(frame[1]) + f64::from(frame[3]) / 2.0,
            ],
        }
    }

    fn fixture_control_value(&self, label: &str) -> String {
        let pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .find(|pid| self.launched.contains(pid))
            .expect("test-owned fixture PID");
        let window = fixture_window(pid);
        let element = native_ax_nodes(&window)
            .into_iter()
            .find(|e| native_ax_string(e, "AXDescription").as_deref() == Some(label))
            .expect("test-owned control");
        native_ax_string(&element, "AXValue").expect("native test value")
    }

    fn turn(&self, active: bool) {
        assert_eq!(
            self.client
                .call(Request::Turn { active }, Duration::from_secs(5))
                .unwrap(),
            Reply::Done
        );
    }

    fn esc_count(&self) -> usize {
        self.esc_events.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn fixture_frontmost(&self, pid: i32) {
        if NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier())
            != Some(pid)
        {
            record_restoration(
                Some(pid),
                CGEvent::location(Some(&CGEvent::new(None).unwrap())),
            );
        }
        assert_eq!(
            NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .unwrap()
                .processIdentifier(),
            pid,
            "fixture must be frontmost before HID keyboard input"
        );
    }

    fn post_esc(&self, pid: i32, marker: i64) {
        use objc2_core_graphics::{CGEventField, CGEventTapLocation};
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(Some(&source), 53, down).unwrap();
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::EventSourceUserData,
                marker,
            );
            assert_eq!(
                NSWorkspace::sharedWorkspace()
                    .frontmostApplication()
                    .unwrap()
                    .processIdentifier(),
                pid,
                "refusing to post HID keyboard input to another app"
            );
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            record_test_hid_event();
        }
        thread::sleep(Duration::from_millis(100));
    }

    fn post_cmd_control(&self, pid: i32, keycode: u16) {
        use objc2_core_graphics::{CGEventField, CGEventFlags, CGEventTapLocation};
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid).unwrap();
        assert!(app.bundleIdentifier().is_some_and(|id| matches!(
            id.to_string().as_str(),
            "dev.orca.cu-fixture" | "com.apple.TextEdit"
        )));
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        for (code, down, flags) in [
            (55, true, CGEventFlags::MaskCommand),
            (keycode, true, CGEventFlags::MaskCommand),
            (keycode, false, CGEventFlags::MaskCommand),
            (55, false, CGEventFlags::empty()),
        ] {
            assert_eq!(
                NSWorkspace::sharedWorkspace()
                    .frontmostApplication()
                    .unwrap()
                    .processIdentifier(),
                pid
            );
            let event = CGEvent::new_keyboard_event(Some(&source), code, down).unwrap();
            CGEvent::set_flags(Some(&event), flags);
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::EventSourceUserData,
                0x4F52_4341,
            );
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            record_test_hid_event();
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(100));
    }

    fn make_finder_frontmost(&mut self) {
        let before = bundle_pids("com.apple.finder");
        let output = Command::new("osascript")
            .args(["-e", "tell application \"Finder\" to activate"])
            .output()
            .unwrap();
        register_launch_pids(
            &mut self.launched,
            &before,
            &bundle_pids("com.apple.finder"),
        );
        assert!(output.status.success());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(app) = NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .filter(|app| {
                    app.bundleIdentifier()
                        .is_some_and(|id| id.to_string() == "com.apple.finder")
                })
            {
                self.background_state = Some((
                    app.processIdentifier(),
                    CGEvent::location(Some(&CGEvent::new(None).unwrap())),
                ));
                break;
            }
            if Instant::now() >= deadline {
                record_gui_failure(
                    GuiFailureKind::StateMismatch,
                    "Finder did not become frontmost".into(),
                );
                let pid = bundle_pids("com.apple.finder")
                    .into_iter()
                    .next()
                    .expect("Finder activation succeeded but Finder is not running");
                self.background_state =
                    Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_log_contains(&self, log: &std::path::Path, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let text = std::fs::read_to_string(log).unwrap();
            if text.contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "missing {needle} in {text}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_frontmost_and_pointer_unchanged(&self) {
        let (pid, pointer) = self.background_state.expect("record Finder state first");
        let actual = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier());
        if actual != Some(pid) {
            record_gui_failure(
                GuiFailureKind::StateMismatch,
                format!("frontmost app changed: expected {pid}, actual {actual:?}"),
            );
        }
        let now = CGEvent::location(Some(&CGEvent::new(None).unwrap()));
        if !pointer_restored(pointer, now) {
            record_gui_failure(
                GuiFailureKind::StateMismatch,
                format!("pointer changed: expected {pointer:?}, actual {now:?}"),
            );
        }
    }

    fn launch_fixture(&mut self) -> (i32, PathBuf) {
        self.launch_fixture_with_flags(&[])
    }

    fn launch_fixture_with_flags(&mut self, flags: &[&str]) -> (i32, PathBuf) {
        let log = self
            .directory
            .path()
            .join(format!("fixture-{}.jsonl", self.launched.len()));
        assert!(
            Command::new("/usr/bin/open")
                .arg("-n")
                .arg(fixture_app())
                .arg("--args")
                .arg(&log)
                .args(flags)
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let pid = std::fs::read_to_string(&log).ok().and_then(|text| {
                let event: serde_json::Value = serde_json::from_str(text.lines().next()?).ok()?;
                (event["type"] == "started")
                    .then(|| event["pid"].as_i64())?
                    .and_then(|pid| i32::try_from(pid).ok())
            });
            if let Some(pid) = pid {
                // The PID receipt comes from this invocation's private log, so
                // cleanup remains exact even if app discovery fails afterwards.
                self.launched.insert(pid);
                if self.apps().into_iter().any(|app| {
                    app.pid == Some(pid)
                        && app
                            .windows
                            .iter()
                            .any(|window| window.title == "Orca CU Fixture")
                }) && NSWorkspace::sharedWorkspace()
                    .frontmostApplication()
                    .is_some_and(|app| app.processIdentifier() == pid)
                {
                    return (pid, log);
                }
            }
            assert!(Instant::now() < deadline, "fixture did not open a window");
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.restore_state {
            return;
        }
        for pid in cleanup_pids(&self.running, &self.launched) {
            if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
                app.terminate();
                let deadline = Instant::now() + Duration::from_secs(3);
                while !app.isTerminated() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(20));
                }
                if !app.isTerminated() {
                    app.forceTerminate();
                }
            }
        }
        record_restoration(self.frontmost, self.pointer);
    }
}

fn bundle_pids(bundle_id: &str) -> HashSet<i32> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id))
        .iter()
        .map(|app| app.processIdentifier())
        .collect()
}

fn register_launch_pids(
    registered: &mut HashSet<i32>,
    before: &HashSet<i32>,
    after: &HashSet<i32>,
) {
    registered.extend(after.difference(before).copied());
}

fn cleanup_pids(running: &HashSet<i32>, launched: &HashSet<i32>) -> Vec<i32> {
    let mut pids: Vec<_> = launched.difference(running).copied().collect();
    pids.sort_unstable();
    pids
}

fn record_restoration(frontmost: Option<i32>, pointer: objc2_core_foundation::CGPoint) {
    use objc2_core_graphics::CGError;
    if let Some(pid) = frontmost {
        let restored = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .is_some_and(|app| {
                app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows)
            });
        if !restored {
            record_gui_failure(
                GuiFailureKind::NativeOperation,
                format!("frontmost activation for PID {pid} failed"),
            );
        } else {
            let deadline = Instant::now() + Duration::from_secs(2);
            while NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(|app| app.processIdentifier())
                != Some(pid)
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
            if NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(|app| app.processIdentifier())
                != Some(pid)
            {
                record_gui_failure(
                    GuiFailureKind::StateMismatch,
                    format!("frontmost PID {pid} was not restored"),
                );
            }
        }
    }
    let warped = CGWarpMouseCursorPosition(pointer);
    if warped != CGError::Success {
        record_gui_failure(
            GuiFailureKind::NativeOperation,
            format!("pointer warp failed: {warped:?}"),
        );
    }
    match CGEvent::new(None).map(|event| CGEvent::location(Some(&event))) {
        Some(actual) if !pointer_restored(pointer, actual) => {
            record_gui_failure(
                GuiFailureKind::StateMismatch,
                format!("pointer not restored: expected {pointer:?}, actual {actual:?}"),
            );
        }
        None => record_gui_failure(
            GuiFailureKind::NativeOperation,
            "Could not read the restored pointer position".into(),
        ),
        Some(_) => {}
    }
}

#[test]
#[ignore]
fn hello_reports_permissions_the_os_and_the_host_terminal() {
    run_live_test(|| {
        let Some(live) = Live::start() else { return };
        assert!(live.hello.macos_major >= 14);
        if let Ok(bundle_id) = std::env::var("__CFBundleIdentifier") {
            assert_eq!(
                live.hello.host_bundle_id.as_deref(),
                Some(bundle_id.as_str())
            );
        }
    });
}

#[test]
#[ignore]
fn list_and_resolve_find_the_fixture_by_name_and_bundle_id() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live.launch_fixture();
        let app = live
            .apps()
            .into_iter()
            .find(|app| app.pid == Some(pid))
            .unwrap();
        assert_eq!(app.bundle_id, "dev.orca.cu-fixture");
        assert!(
            app.windows
                .iter()
                .any(|window| window.title == "Orca CU Fixture")
        );
        let resolve = |name: &str| {
            live.client
                .call(
                    Request::ResolveApp {
                        app: name.into(),
                        allow_not_running: false,
                    },
                    Duration::from_secs(30),
                )
                .unwrap()
        };
        assert_eq!(resolve("CUFixture"), Reply::Resolved { app: app.clone() });
        assert_eq!(resolve("dev.orca.cu-fixture"), Reply::Resolved { app });
    });
}

#[test]
#[ignore]
fn a_second_driver_finds_the_session_lock_held() {
    run_live_test(|| {
        use orca_computer_use::serve::{DriverHandler, LineWriter};
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live.launch_fixture();
        assert!(
            live.apps().iter().any(|app| app.pid == Some(pid)),
            "owner can list apps"
        );
        let before = bundle_pids("dev.orca.cu-fixture");
        let launched = live.client.call(
            Request::LaunchApp {
                app: "dev.orca.cu-fixture".into(),
                open: vec![],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("dev.orca.cu-fixture"),
        );
        let launched = launched.unwrap();
        assert!(
            matches!(launched, Reply::Launched { app } if app.pid == Some(pid)),
            "owner can launch its fixture"
        );
        let mut second = MacDriver::new(live.directory.path().join("computer-use.lock"));
        let output = LineWriter::new(std::io::sink());
        let requests = || {
            [
                Request::ListApps,
                Request::LaunchApp {
                    app: "dev.orca.cu-fixture".into(),
                    open: vec![],
                },
            ]
        };
        for request in requests() {
            let before = bundle_pids("dev.orca.cu-fixture");
            let result = second.handle(request, &output);
            register_launch_pids(
                &mut live.launched,
                &before,
                &bundle_pids("dev.orca.cu-fixture"),
            );
            let error = result.unwrap_err();
            assert_eq!(error.kind, ErrorKind::LockHeld);
            assert_eq!(error.message, "another Orca session is using the computer");
        }
        let error = second
            .handle(Request::Hello { protocol: 1 }, &output)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::LockHeld);
        for request in requests() {
            let before = bundle_pids("dev.orca.cu-fixture");
            let result = second.handle(request, &output);
            register_launch_pids(
                &mut live.launched,
                &before,
                &bundle_pids("dev.orca.cu-fixture"),
            );
            let error = result.unwrap_err();
            assert_eq!(error.kind, ErrorKind::LockHeld);
            assert_eq!(error.message, "another Orca session is using the computer");
        }
    });
}

#[test]
#[ignore]
fn launch_app_opens_a_file_in_textedit() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let file = live.directory.path().join("orca-cu-test.txt");
        std::fs::write(&file, "Orca computer use fixture document\n").unwrap();
        live.make_finder_frontmost();
        let before = bundle_pids("com.apple.TextEdit");
        let result = live.client.call(
            Request::LaunchApp {
                app: "com.apple.TextEdit".into(),
                open: vec![file.to_string_lossy().into_owned()],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("com.apple.TextEdit"),
        );
        let Reply::Launched { app } = result.unwrap() else {
            panic!("expected launched app")
        };
        live.assert_frontmost_and_pointer_unchanged();
        assert_eq!(app.bundle_id, "com.apple.TextEdit");
        assert!(app.pid.is_some());
        assert!(
            !app.windows.is_empty(),
            "TextEdit opened the temporary file"
        );
    });
}

#[test]
#[ignore]
fn opening_a_nonexistent_file_in_already_running_textedit_returns_an_error() {
    run_live_test(|| {
        use orca_computer_use::client::CallError;
        let Some(mut live) = Live::start() else {
            return;
        };
        if !NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
            "com.apple.TextEdit",
        ))
        .is_empty()
        {
            eprintln!("skipped: TextEdit was already running");
            return;
        }
        let file = live.directory.path().join("native-open-control.txt");
        std::fs::write(&file, "Native launch completion control\n").unwrap();
        let before = bundle_pids("com.apple.TextEdit");
        let result = live.client.call(
            Request::LaunchApp {
                app: "com.apple.TextEdit".into(),
                open: vec![file.to_string_lossy().into_owned()],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("com.apple.TextEdit"),
        );
        let Reply::Launched { app } = result.unwrap() else {
            panic!("expected successful launch control")
        };
        assert!(
            !app.windows.is_empty(),
            "valid file opens in the same TextEdit process"
        );
        let missing = live.directory.path().join("does-not-exist.txt");
        let before = bundle_pids("com.apple.TextEdit");
        let result = live.client.call(
            Request::LaunchApp {
                app: "com.apple.TextEdit".into(),
                open: vec![missing.to_string_lossy().into_owned()],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("com.apple.TextEdit"),
        );
        let Err(CallError::Driver(error)) = result else {
            panic!("native open failure was hidden: {result:?}")
        };
        assert_eq!(error.kind, ErrorKind::Failed);
        assert!(!error.message.is_empty());
    });
}

#[test]
#[ignore]
fn cleanup_only_selects_explicitly_launched_processes() {
    run_live_test(|| {
        let running = HashSet::from([10]);
        // PID 77 represents an unrelated app that appeared during the test.
        let launched = HashSet::from([10, 42]);
        let pids = cleanup_pids(&running, &launched);
        assert_eq!(pids, vec![42]);
        assert!(!pids.contains(&77));
    });
}

#[test]
#[ignore]
fn cleanup_preserves_an_app_launched_by_another_live_session() {
    run_live_test(|| {
        let Some(mut owner) = Live::start() else {
            return;
        };
        let Some(other) = Live::start() else { return };
        // The fixture appears after `other` captured its starting process list,
        // but only `owner` launched it and received its private PID receipt.
        let (pid, _) = owner.launch_fixture();
        assert!(
            owner.apps().iter().any(|app| app.pid == Some(pid)),
            "fixture is running before cleanup"
        );
        drop(other);
        assert!(
            owner.apps().iter().any(|app| app.pid == Some(pid)),
            "cleanup must leave another session's app running"
        );
    });
}

#[test]
#[ignore]
fn launch_attribution_registers_only_pids_that_appeared_during_the_request() {
    run_live_test(|| {
        let before = HashSet::from([10, 20]);
        let after = HashSet::from([20, 42, 43]);
        let mut registered = HashSet::from([7]);
        register_launch_pids(&mut registered, &before, &after);
        assert_eq!(registered, HashSet::from([7, 42, 43]));
        assert!(
            !registered.contains(&20),
            "pre-request process is never claimed"
        );
        assert!(
            !registered.contains(&10),
            "exited pre-request process is never claimed"
        );
        register_launch_pids(&mut registered, &after, &after);
        assert_eq!(
            registered,
            HashSet::from([7, 42, 43]),
            "a request with no new process claims nothing"
        );
    });
}

#[test]
#[ignore]
fn observe_lists_the_fixture_controls_in_screenshot_pixels() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        live.launch_fixture();
        let observation = live.observe("dev.orca.cu-fixture", None);
        let shot = observation.screenshot.as_ref().expect("screenshot");
        assert!(shot.width > 0 && shot.height > 0 && shot.width <= 1280 && shot.height <= 1280);
        use base64::Engine;
        let png = base64::engine::general_purpose::STANDARD
            .decode(&shot.png_base64)
            .unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (shot.width, shot.height)
        );
        let labels: Vec<_> = observation
            .elements
            .iter()
            .filter_map(|e| e.label.as_deref())
            .collect();
        for expected in ["Increment", "Name", "Password", "List"] {
            assert!(
                labels.contains(&expected),
                "{expected} missing from {labels:?}"
            );
        }
        let password = observation
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Password"))
            .unwrap();
        assert!(password.secure && password.value.is_none());
        assert!(!observation.no_element_tree);
        for (index, element) in observation.elements.iter().enumerate() {
            assert_eq!(element.id as usize, index + 1);
            let [x, y, w, h] = element.frame;
            assert!(
                x >= 0
                    && y >= 0
                    && w >= 0
                    && h >= 0
                    && x + w <= shot.width as i32 + 2
                    && y + h <= shot.height as i32 + 2,
                "{element:?}"
            );
        }
        let capped = live.observe_window(
            "dev.orca.cu-fixture",
            Some(observation.window.id),
            None,
            1,
            false,
        );
        assert!(capped.screenshot.is_none());
        assert_eq!(capped.elements, observation.elements[..1]);
        assert_eq!(capped.omitted as usize, observation.elements.len() - 1);
    });
}

#[test]
#[ignore]
fn observe_with_a_query_keeps_matches_and_their_ancestors() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        live.launch_fixture();
        let all = live.observe("dev.orca.cu-fixture", None);
        let expected = orca_computer_use::observation::filter_elements(&all.elements, "incr");
        assert!(!expected.is_empty(), "positive match control");
        let filtered = live.observe("dev.orca.cu-fixture", Some("incr"));
        let mut expected = expected;
        for (i, e) in expected.iter_mut().enumerate() {
            e.id = i as u32 + 1;
        }
        assert_eq!(filtered.elements, expected);
        assert!(
            filtered
                .elements
                .iter()
                .any(|e| e.label.as_deref() == Some("Increment"))
        );
        let descendants = live.observe("dev.orca.cu-fixture", Some("line 1"));
        assert!(descendants.elements.iter().any(
            |e| e.role == "text area" && e.value.as_ref().is_some_and(|v| v.contains("line 1"))
        ));
        assert!(
            descendants
                .elements
                .iter()
                .any(|e| e.label.as_deref() == Some("List")),
            "the retained scroll area is an ancestor of matching text"
        );
        assert!(
            !descendants
                .elements
                .iter()
                .any(|e| e.label.as_deref() == Some("Increment")),
            "an unrelated sibling is excluded"
        );
        assert!(
            live.observe("dev.orca.cu-fixture", Some("no-such-fixture-control-xyz"))
                .elements
                .is_empty()
        );
    });
}

struct FinderWindow(i32);
impl Drop for FinderWindow {
    fn drop(&mut self) {
        let output = Command::new("osascript")
            .args([
                "-e",
                &format!("tell application \"Finder\" to close window id {}", self.0),
            ])
            .output()
            .unwrap();
        if !output.status.success() {
            RESTORATION_FAILURES.lock().unwrap().push(format!(
                "close test Finder window: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
}

#[test]
#[ignore]
fn observe_captures_a_window_covered_by_another() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        live.launch_fixture();
        let before = live.observe("dev.orca.cu-fixture", None);
        let finder_before = bundle_pids("com.apple.finder");
        let output = Command::new("osascript")
            .args([
                "-e",
                "tell application \"Finder\"
set probe to make new Finder window
return id of probe
end tell",
            ])
            .output()
            .unwrap();
        register_launch_pids(
            &mut live.launched,
            &finder_before,
            &bundle_pids("com.apple.finder"),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let window = FinderWindow(
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        );
        let output = Command::new("osascript").args(["-e", &format!("tell application \"Finder\"\nset bounds of window id {} to {{100, 100, 1100, 1000}}\nactivate\nend tell", window.0)]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if NSWorkspace::new()
                .frontmostApplication()
                .and_then(|app| app.bundleIdentifier())
                .is_some_and(|id| id.to_string() == "com.apple.finder")
            {
                break;
            }
            if Instant::now() >= deadline {
                record_gui_failure(
                    GuiFailureKind::StateMismatch,
                    "Finder must actually cover the fixture".into(),
                );
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let covered = live.observe_window(
            "dev.orca.cu-fixture",
            Some(before.window.id),
            None,
            300,
            true,
        );
        let before = before.screenshot.unwrap();
        let covered = covered.screenshot.unwrap();
        assert_eq!(
            (covered.width, covered.height),
            (before.width, before.height)
        );
        use base64::Engine;
        let decode = |s: &str| {
            image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(s).unwrap())
                .unwrap()
                .to_rgba8()
        };
        // The fixture's teal canvas is a stable positive control even when covered.
        let x = before.width * 3 / 4;
        let y = before.height / 2;
        let control = *decode(&before.png_base64).get_pixel(x, y);
        assert!(
            control[1] > control[0] && control[2] > control[0],
            "expected teal canvas: {control:?}"
        );
        assert_eq!(*decode(&covered.png_base64).get_pixel(x, y), control);
    });
}

#[test]
#[ignore]
fn observe_of_an_electron_app_waits_for_its_tree() {
    run_live_test(|| {
        let Ok(bundle) = std::env::var("ORCA_CU_ELECTRON_APP") else {
            eprintln!("skipped: ORCA_CU_ELECTRON_APP is unset");
            return;
        };
        let Some(mut live) = Live::start() else {
            return;
        };
        if !bundle_pids(&bundle).is_empty() {
            eprintln!("skipped: Electron app already running");
            return;
        }
        live.launch_fixture();
        let control = live.observe("dev.orca.cu-fixture", None);
        assert!(control.elements.iter().any(|e| e.role == "button"));
        assert!(control.elements.iter().any(|e| e.role == "text field"));
        let before = bundle_pids(&bundle);
        let result = live.client.call(
            Request::LaunchApp {
                app: bundle.clone(),
                open: vec![],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(&mut live.launched, &before, &bundle_pids(&bundle));
        let Reply::Launched { app } = result.unwrap() else {
            panic!("expected launched Electron app");
        };
        let observation = live.observe(&bundle, None);
        eprintln!(
            "Electron {bundle}: no_element_tree={}, elements={}, buttons={}, text_fields={}; fixture control has both roles",
            observation.no_element_tree,
            observation.elements.len(),
            observation
                .elements
                .iter()
                .filter(|e| e.role == "button")
                .count(),
            observation
                .elements
                .iter()
                .filter(|e| e.role == "text field")
                .count()
        );
        let frame_buttons = title_bar_button_frames(app.pid.unwrap(), &observation);
        assert!(
            observation
                .elements
                .iter()
                .any(|element| element.role != "button" || !frame_buttons.contains(&element.frame)),
            "Electron observation must contain content beyond the window's title-bar buttons"
        );
        assert!(!observation.no_element_tree);
        assert!(
            observation
                .elements
                .iter()
                .any(|e| e.role == "button" || e.role == "text field")
        );
    });
}

#[test]
#[ignore]
fn observe_stocks_after_dismissing_its_welcome_page() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        // Positive control for any missing Catalyst roles, on this same driver.
        live.launch_fixture();
        let control = live.observe("dev.orca.cu-fixture", None);
        assert!(control.elements.iter().any(|e| e.role == "button"));
        assert!(control.elements.iter().any(|e| e.role == "text field"));
        let before = bundle_pids("com.apple.stocks");
        if !before.is_empty() {
            eprintln!("skipped: Stocks already running");
            return;
        }
        let result = live.client.call(
            Request::LaunchApp {
                app: "com.apple.stocks".into(),
                open: vec![],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("com.apple.stocks"),
        );
        result.unwrap();
        // Stocks' first-run welcome view loads asynchronously after its window.
        thread::sleep(Duration::from_secs(20));
        let mut observation = live.observe("com.apple.stocks", None);
        use base64::Engine as _;
        std::fs::write(
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("stocks-before.png"),
            base64::engine::general_purpose::STANDARD
                .decode(&observation.screenshot.as_ref().unwrap().png_base64)
                .unwrap(),
        )
        .unwrap();
        let continued = if let Some(button) = observation.elements.iter().find(|e| {
            e.role == "button"
                && e.label
                    .as_deref()
                    .is_some_and(|label| label == "Continue" || label == "继续")
        }) {
            live.act_app(
                "com.apple.stocks",
                Some(observation.window.id),
                click_target(Target::Element { element: button.id }),
                false,
                true,
            )
            .unwrap();
            thread::sleep(Duration::from_millis(500));
            observation = live.observe("com.apple.stocks", None);
            true
        } else {
            false
        };
        let buttons = observation
            .elements
            .iter()
            .filter(|e| e.role == "button")
            .count();
        let fields = observation
            .elements
            .iter()
            .filter(|e| e.role == "text field")
            .count();
        eprintln!(
            "Stocks: dismissed Continue={continued}, no_element_tree={}, elements={}, buttons={buttons}, text_fields={fields}; fixture control has both roles",
            observation.no_element_tree,
            observation.elements.len()
        );
    });
}

fn click_target(target: Target) -> Action {
    Action::Click {
        target,
        button: Button::Left,
        count: 1,
        modifiers: vec![],
    }
}
fn element_named(observation: &Observation, name: &str) -> Target {
    Target::Element {
        element: observation
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some(name))
            .unwrap()
            .id,
    }
}
fn background_fixture() -> Option<(Live, PathBuf, Observation)> {
    let mut live = Live::start()?;
    let (_, log) = live.launch_fixture();
    live.make_finder_frontmost();
    let observation = live.observe("dev.orca.cu-fixture", None);
    Some((live, log, observation))
}
#[test]
#[ignore]
fn clicking_an_element_presses_it_in_the_background() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let outcome = live
            .act(click_target(element_named(&obs, "Increment")), false)
            .unwrap();
        assert!(!outcome.foreground_used);
        assert!(outcome.observation.is_some());
        live.assert_log_contains(&log, r#""type":"click""#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}
#[test]
#[ignore]
fn typing_goes_into_the_focused_field_in_the_background() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        live.act(
            Action::Type {
                text: "".into(),
                target: Some(element_named(&obs, "Name")),
            },
            false,
        )
        .unwrap();
        live.act(
            Action::Type {
                text: "abc".into(),
                target: None,
            },
            false,
        )
        .unwrap();
        live.assert_log_contains(&log, r#""value":"abc""#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}
/// The current keyboard input source's ID and the languages it declares.
fn current_keyboard_input_source() -> Option<(String, Vec<String>)> {
    use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> *mut CFType;
        fn TISGetInputSourceProperty(
            source: *const CFType,
            key: *const CFString,
        ) -> *mut std::ffi::c_void;
        static kTISPropertyInputSourceID: *const CFString;
        static kTISPropertyInputSourceLanguages: *const CFString;
    }
    // Copy owns the source; its properties follow the borrowed Get rule.
    let source = unsafe { std::ptr::NonNull::new(TISCopyCurrentKeyboardInputSource())? };
    let source = unsafe { CFRetained::from_raw(source) };
    let property = |key| {
        let value = unsafe { TISGetInputSourceProperty(&*source, key) };
        (!value.is_null()).then(|| unsafe { &*value.cast::<CFType>() })
    };
    let id = property(unsafe { kTISPropertyInputSourceID })?
        .downcast_ref::<CFString>()?
        .to_string();
    let languages = property(unsafe { kTISPropertyInputSourceLanguages })
        .and_then(|value| value.downcast_ref::<CFArray>())
        .map(|array| {
            (0..array.len())
                .filter_map(|i| {
                    let value = unsafe { &*array.value_at_index(i as isize).cast::<CFType>() };
                    value.downcast_ref::<CFString>().map(ToString::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    Some((id, languages))
}

#[test]
#[ignore]
fn typing_with_a_chinese_input_method_enters_the_text_exactly() {
    run_live_test(|| {
        let source = current_keyboard_input_source();
        eprintln!("current keyboard input source: {source:?}");
        // An input method is Chinese when its primary language is (Apple's
        // SCIM/TCIM and third-party ones such as WeType or Sogou alike).
        let chinese = source.as_ref().is_some_and(|(id, languages)| {
            languages
                .first()
                .is_some_and(|language| language == "zh" || language.starts_with("zh-"))
                && !id.starts_with("com.apple.keylayout.")
        });
        if !chinese {
            eprintln!(
                "skipped: requires the user to select a Chinese input method; the test does not switch input sources"
            );
            return;
        }
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        live.act(
            Action::Type {
                text: "abc 中文".into(),
                target: Some(element_named(&obs, "Name")),
            },
            false,
        )
        .unwrap();
        live.assert_log_contains(&log, r#""value":"abc 中文""#);
        let events = std::fs::read_to_string(&log).unwrap();
        let last_value = events
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .rfind(|event| event["type"] == "input")
            .unwrap();
        assert_eq!(
            last_value["value"], "abc 中文",
            "logged input must equal the requested text exactly"
        );
        assert_eq!(live.fixture_control_value("Name"), "abc 中文");
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn typing_into_the_password_field_needs_approval_first() {
    run_live_test(|| {
        let Some((live, _, obs)) = background_fixture() else {
            return;
        };
        let action = Action::Type {
            text: "secret".into(),
            target: Some(element_named(&obs, "Password")),
        };
        assert_eq!(
            live.act(action.clone(), false).unwrap_err().kind,
            ErrorKind::NeedsSecureApproval
        );
        assert!(!live.act(action, true).unwrap().foreground_used);
        live.assert_frontmost_and_pointer_unchanged();
    });
}
#[test]
#[ignore]
fn a_point_on_the_button_presses_it() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let frame = obs
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Increment"))
            .unwrap()
            .frame;
        live.act(
            click_target(Target::Point {
                point: [
                    f64::from(frame[0]) + f64::from(frame[2]) / 2.0,
                    f64::from(frame[1]) + f64::from(frame[3]) / 2.0,
                ],
            }),
            false,
        )
        .unwrap();
        live.assert_log_contains(&log, r#""count":1"#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}
#[test]
#[ignore]
fn a_point_on_the_canvas_needs_the_foreground() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        live.act(click_target(element_named(&obs, "Increment")), false)
            .unwrap();
        live.assert_log_contains(&log, r#""type":"click""#);
        let image = live
            .observe("dev.orca.cu-fixture", None)
            .screenshot
            .unwrap();
        assert_eq!(
            live.act(
                click_target(Target::Point {
                    point: [f64::from(image.width) * 0.75, f64::from(image.height) * 0.5]
                }),
                false
            )
            .unwrap_err()
            .kind,
            ErrorKind::NeedsForeground
        );
        assert!(
            !std::fs::read_to_string(log)
                .unwrap()
                .contains("canvas_click")
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}
#[test]
#[ignore]
fn scrolling_the_list_moves_it_in_the_background() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let outcome = live
            .act(
                Action::Scroll {
                    target: element_named(&obs, "List"),
                    direction: Direction::Down,
                    amount: 3,
                },
                false,
            )
            .unwrap();
        assert!(outcome.scroll_position.unwrap() > 0.0);
        live.assert_log_contains(&log, r#""type":"scroll""#);
        assert!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .any(|v| v["type"] == "scroll" && v["y"].as_f64().is_some_and(|y| y > 0.0))
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}
#[test]
#[ignore]
fn an_action_after_the_window_moved_is_stale() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        live.act(click_target(element_named(&obs, "Increment")), false)
            .unwrap();
        live.assert_log_contains(&log, r#""type":"click""#);
        let output = Command::new("osascript").args(["-e", "tell application \"System Events\" to tell process \"CUFixture\" to set position of window 1 to {250, 250}"]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let err = live
            .act(click_target(element_named(&obs, "Increment")), false)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Stale);
        assert_eq!(
            err.message,
            "the window changed since the last observe; observe again"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}
#[test]
#[ignore]
fn cmd_a_reaches_the_focused_field() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        live.act(
            Action::Type {
                text: "abc".into(),
                target: Some(element_named(&obs, "Name")),
            },
            false,
        )
        .unwrap();
        live.assert_log_contains(&log, r#""value":"abc""#);
        let outcome = live
            .act_with_fallback(
                Action::Key {
                    keys: vec!["cmd".into(), "a".into()],
                    target: None,
                },
                false,
                true,
            )
            .unwrap();
        live.act(
            Action::Type {
                text: "x".into(),
                target: None,
            },
            false,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        let observation = live.observe("dev.orca.cu-fixture", None);
        assert!(
            observation
                .elements
                .iter()
                .any(|e| e.label.as_deref() == Some("Name"))
        );
        let value = live.fixture_control_value("Name");
        live.assert_frontmost_and_pointer_unchanged();
        // Positive control: foreground HID reaches the same focused field.
        let pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .next()
            .unwrap();
        live.fixture_frontmost(pid);
        live.post_cmd_control(pid, 0); // A
        live.act(
            Action::Type {
                text: "x".into(),
                target: None,
            },
            false,
        )
        .unwrap();
        live.assert_log_contains(&log, r#""value":"x""#);
        assert_eq!(
            value, "x",
            "Cmd+A must select the field when called from the background"
        );
        assert!(outcome.foreground_used);
    });
}

fn file_changed(file: &std::path::Path, before: std::time::SystemTime) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if std::fs::metadata(file).unwrap().modified().unwrap() != before {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore]
fn cmd_s_saves_textedit_in_the_background() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let file = live.directory.path().join("orca-cu-cmd-s.txt");
        std::fs::write(&file, "Orca Cmd+S probe\n").unwrap();
        let before = bundle_pids("com.apple.TextEdit");
        if !before.is_empty() {
            eprintln!("skipped: TextEdit already running; do not touch the user's documents");
            return;
        }
        let launched = live.client.call(
            Request::LaunchApp {
                app: "com.apple.TextEdit".into(),
                open: vec![file.to_string_lossy().into_owned()],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("com.apple.TextEdit"),
        );
        let Reply::Launched { app } = launched.unwrap() else {
            panic!("expected launched TextEdit");
        };
        let window = app
            .windows
            .iter()
            .find(|w| w.title.contains("orca-cu-cmd-s.txt"))
            .expect("own document window")
            .id;
        live.make_finder_frontmost();
        let observed = live.observe_window("com.apple.TextEdit", Some(window), None, 300, false);
        let text_area = observed
            .elements
            .iter()
            .find(|e| e.role == "text area")
            .expect("observed TextEdit editor")
            .id;
        live.act_app(
            "com.apple.TextEdit",
            Some(window),
            Action::Type {
                text: "Edited by Orca".into(),
                target: Some(Target::Element { element: text_area }),
            },
            false,
            true,
        )
        .unwrap();
        let native_window = native_window_named(app.pid.unwrap(), "orca-cu-cmd-s.txt");
        let native_text = native_ax_nodes(&native_window)
            .into_iter()
            .find(|e| native_ax_string(e, "AXRole").as_deref() == Some("AXTextArea"))
            .unwrap();
        let edited_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if native_ax_string(&native_text, "AXValue")
                .is_some_and(|v| v.contains("Edited by Orca"))
            {
                break;
            }
            assert!(
                Instant::now() < edited_deadline,
                "native typing positive control did not dirty our own document"
            );
            thread::sleep(Duration::from_millis(20));
        }
        live.assert_frontmost_and_pointer_unchanged();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        let action = Action::Key {
            keys: vec!["cmd".into(), "s".into()],
            target: None,
        };
        let outcome = live
            .act_app(
                "com.apple.TextEdit",
                Some(window),
                action.clone(),
                false,
                true,
            )
            .unwrap();
        let saved = file_changed(&file, mtime);
        live.assert_frontmost_and_pointer_unchanged();
        // Positive control: marked HID Cmd+S saves the same dirty document.
        record_restoration(
            app.pid,
            CGEvent::location(Some(&CGEvent::new(None).unwrap())),
        );
        let control_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        live.act_app(
            "com.apple.TextEdit",
            Some(window),
            Action::Type {
                text: " control".into(),
                target: None,
            },
            false,
            true,
        )
        .unwrap();
        live.post_cmd_control(app.pid.unwrap(), 1); // S
        assert!(
            file_changed(&file, control_mtime),
            "positive control: frontmost Cmd+S changes the file mtime"
        );
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("Edited by Orca")
        );
        assert!(saved, "background Cmd+S did not change the file mtime");
        assert!(outcome.foreground_used);
    });
}

// Seconds are measured from the start of an attempt. Task 8's marked HID
// posting helpers will record the time of their last event on the same clock.
fn failure_is_inconclusive(
    kind: GuiFailureKind,
    elapsed: f64,
    idle: f64,
    last_own_hid: Option<f64>,
    driver_hid: Option<f64>,
) -> bool {
    if kind == GuiFailureKind::NativeOperation {
        return false;
    }
    let last_own_hid = last_own_hid
        .into_iter()
        .chain(driver_hid)
        .max_by(f64::total_cmp);
    let last_input = elapsed - idle;
    last_input > 0.0 && last_own_hid.is_none_or(|own| last_input > own)
}

#[test]
fn hid_activity_distinguishes_user_interference_from_a_regression() {
    let state = GuiFailureKind::StateMismatch;
    assert!(failure_is_inconclusive(state, 10.0, 2.0, None, None));
    assert!(!failure_is_inconclusive(state, 10.0, 10.0, None, None));
    assert!(!failure_is_inconclusive(state, 10.0, 12.0, None, None));
    assert!(!failure_is_inconclusive(state, 10.0, 2.0, Some(8.0), None));
    assert!(!failure_is_inconclusive(state, 10.0, 3.0, Some(8.0), None));
    assert!(failure_is_inconclusive(state, 10.0, 1.0, Some(8.0), None));
    // Even user input after the attempt started cannot excuse a native error.
    assert!(!failure_is_inconclusive(
        GuiFailureKind::NativeOperation,
        10.0,
        2.0,
        None,
        None,
    ));
}

#[test]
#[ignore]
fn a_point_on_a_hidden_button_needs_the_foreground() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let frame = obs
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Increment"))
            .unwrap()
            .frame;
        let action = click_target(Target::Point {
            point: [
                f64::from(frame[0]) + f64::from(frame[2]) / 2.0,
                f64::from(frame[1]) + f64::from(frame[3]) / 2.0,
            ],
        });
        live.act(action.clone(), false).unwrap();
        live.assert_log_contains(&log, r#""count":1"#);
        let receipt: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let pid = i32::try_from(receipt["pid"].as_i64().unwrap()).unwrap();
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid).unwrap();
        let output = Command::new("osascript")
            .args(["-e", "tell application \"System Events\" to set visible of process \"CUFixture\" to false"])
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.isHidden() {
            assert!(Instant::now() < deadline, "fixture did not become hidden");
            thread::sleep(Duration::from_millis(20));
        }
        let error = live.act(action, false).unwrap_err();
        assert_eq!(error.kind, ErrorKind::NeedsForeground);
        assert_eq!(
            error.message,
            "The app is hidden; the foreground fallback is turned off"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn a_canvas_click_uses_the_foreground_and_restores_it() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        // Positive control: the same HID canvas action works while the fixture
        // is frontmost. The background case below also has to restore Finder.
        let fixture_pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .next()
            .unwrap();
        live.fixture_frontmost(fixture_pid);
        let action = click_target(live.point_inside(&obs, "canvas"));
        assert!(
            live.act_with_fallback(action.clone(), false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
        std::fs::write(&log, "").unwrap();
        let mut live = live;
        live.make_finder_frontmost();
        let disabled = live.act(action.clone(), false).unwrap_err();
        assert_eq!(disabled.kind, ErrorKind::NeedsForeground);
        assert!(
            disabled
                .message
                .ends_with("; the foreground fallback is turned off")
        );
        let original_name = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .unwrap()
            .localizedName()
            .unwrap()
            .to_string();
        let outcome = live.act_with_fallback(action, false, true).unwrap();
        assert!(outcome.foreground_used);
        assert_eq!(
            outcome.note,
            Some(format!(
                "used the foreground (switched back to {original_name})"
            ))
        );
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
        live.assert_frontmost_and_pointer_unchanged();
        // Task 8's restore failure followed a HID click into Password from Finder.
        let password = click_target(element_named(&obs, "Password"));
        assert!(
            live.act_with_fallback(password, false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn two_foreground_actions_in_a_row_do_not_count_as_user_activity() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let action = click_target(live.point_inside(&obs, "canvas"));
        // Positive control: the first click succeeds from the same Finder state.
        assert!(
            live.act_with_fallback(action.clone(), false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
        live.assert_frontmost_and_pointer_unchanged();
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        let outcome = live.act_with_fallback(action, false, true).unwrap();
        assert!(outcome.foreground_used);
        let clicks = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .filter(|e| e["type"] == "canvas_click")
            .count();
        assert_eq!(clicks, 2, "both canvas clicks reached the fixture");
        eprintln!(
            "second foreground click finished in {:?}; no UserActive",
            started.elapsed()
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

fn esc_fixture() -> Option<(Live, i32, PathBuf)> {
    let mut live = Live::start()?;
    let (pid, log) = live.launch_fixture();
    live.fixture_frontmost(pid);
    let obs = live.observe("dev.orca.cu-fixture", None);
    live.act(
        Action::Type {
            text: "".into(),
            target: Some(element_named(&obs, "Name")),
        },
        false,
    )
    .unwrap();
    Some((live, pid, log))
}

fn log_has_esc(log: &std::path::Path) -> bool {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|event| event["type"] == "keydown" && event["chars"] == "\u{1b}")
}

#[test]
#[ignore]
fn the_esc_listener_swallows_a_user_esc_and_reports_it() {
    run_live_test(|| {
        let Some((live, pid, log)) = esc_fixture() else {
            return;
        };
        live.turn(true);
        live.turn(true); // Installing twice must not duplicate the callback.
        live.post_esc(pid, 0x4F52_4341);
        assert!(
            log_has_esc(&log),
            "positive control: marked Esc reaches the focused fixture"
        );
        std::fs::write(&log, "").unwrap();
        live.post_esc(pid, 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while live.esc_count() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(live.esc_count(), 1);
        assert!(!log_has_esc(&log));
        live.turn(false);
    });
}

#[test]
#[ignore]
fn the_drivers_own_esc_passes_the_listener() {
    run_live_test(|| {
        let Some((live, pid, log)) = esc_fixture() else {
            return;
        };
        live.turn(true);
        live.post_esc(pid, 0x4F52_4341);
        assert!(log_has_esc(&log));
        assert_eq!(live.esc_count(), 0);
        live.post_esc(pid, 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while live.esc_count() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(live.esc_count(), 1, "positive control: listener is active");
        live.turn(false);
    });
}

#[test]
#[ignore]
fn the_listener_is_gone_after_the_turn_ends() {
    run_live_test(|| {
        let Some((live, pid, log)) = esc_fixture() else {
            return;
        };
        live.turn(true);
        live.post_esc(pid, 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while live.esc_count() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(live.esc_count(), 1, "positive control: listener was active");
        live.turn(false);
        live.turn(false);
        live.post_esc(pid, 0);
        assert!(log_has_esc(&log));
        assert_eq!(live.esc_count(), 1);
    });
}

#[test]
#[ignore]
fn a_busy_user_makes_the_fallback_give_up() {
    run_live_test(|| {
        use objc2_core_graphics::{CGEventTapLocation, CGMouseButton};
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let action = click_target(live.point_inside(&obs, "canvas"));
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        let point = CGEvent::location(Some(&CGEvent::new(None).unwrap()));
        let movement = CGEvent::new_mouse_event(
            Some(&source),
            CGEventType::MouseMoved,
            point,
            CGMouseButton::Left,
        )
        .unwrap();
        let post = || {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&movement));
            record_test_hid_event();
        };
        post();
        thread::sleep(Duration::from_millis(20));
        // Sustain activity through the three-second wait; one movement alone
        // becomes idle after 0.5s and is required to permit the fallback.
        let result = thread::scope(|scope| {
            let (send, receive) = std::sync::mpsc::channel();
            let live = &live;
            let action = action.clone();
            scope.spawn(move || {
                send.send(live.act_with_fallback(action, false, true))
                    .unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(result) = receive.try_recv() {
                    break result;
                }
                assert!(Instant::now() < deadline, "busy fallback did not finish");
                post();
                thread::sleep(Duration::from_millis(50));
            }
        });
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::UserActive);
        assert_eq!(
            error.message,
            "the user is typing or moving the mouse; try again in a moment"
        );
        live.assert_frontmost_and_pointer_unchanged();
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("canvas_click")
        );
        thread::sleep(Duration::from_millis(550));
        assert!(
            live.act_with_fallback(action, false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn a_double_canvas_click_uses_the_foreground() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let action = Action::Click {
            target: live.point_inside(&obs, "canvas"),
            button: Button::Left,
            count: 2,
            modifiers: vec!["shift".into()],
        };
        assert!(
            live.act_with_fallback(action, false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_log_contains(&log, r#""clicks":2"#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn a_failed_foreground_action_restores_the_app_and_pointer() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let target = live.point_inside(&obs, "canvas");
        let error = live
            .act_with_fallback(
                Action::Click {
                    target,
                    button: Button::Left,
                    count: 1,
                    modifiers: vec!["invalid".into()],
                },
                false,
                true,
            )
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Failed);
        assert!(error.message.contains("unknown key"));
        live.assert_frontmost_and_pointer_unchanged();
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("canvas_click")
        );
        assert!(
            live.act_with_fallback(click_target(target), false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn esc_stops_a_foreground_action_and_releases_it() {
    run_live_test(|| {
        use objc2_core_graphics::CGEventFlags;
        let Some((mut live, pid, log)) = esc_fixture() else {
            return;
        };
        live.make_finder_frontmost();
        let obs = live.observe("dev.orca.cu-fixture", None);
        let target = live.point_inside(&obs, "canvas");
        // Modified double clicks always require the foreground.
        let shift_before = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState)
            .contains(CGEventFlags::MaskShift);
        live.turn(true);
        let control = live
            .act_with_fallback(
                Action::Click {
                    target,
                    button: Button::Left,
                    count: 2,
                    modifiers: vec!["shift".into()],
                },
                false,
                true,
            )
            .unwrap();
        assert!(control.foreground_used);
        live.assert_log_contains(&log, r#""clicks":2"#);
        live.assert_frontmost_and_pointer_unchanged();
        std::fs::write(&log, "").unwrap();
        let result = thread::scope(|scope| {
            let driver = &live;
            let pending = scope.spawn(move || {
                driver.act_with_fallback(
                    Action::Click {
                        target,
                        button: Button::Left,
                        count: 2,
                        modifiers: vec!["shift".into()],
                    },
                    false,
                    true,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(4);
            // Wait for actual delivery of the first down, then stop between
            // events while the driver still owns the button and Shift key.
            while !std::fs::read_to_string(&log)
                .unwrap()
                .contains("canvas_click")
            {
                assert!(
                    Instant::now() < deadline,
                    "foreground click was not delivered"
                );
                thread::sleep(Duration::from_millis(1));
            }
            live.post_esc(pid, 0);
            pending.join().unwrap()
        });
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Failed);
        assert_eq!(error.message, "the turn was stopped by Esc");
        assert_eq!(live.esc_count(), 1);
        assert!(!log_has_esc(&log));
        assert_eq!(
            CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState)
                .contains(CGEventFlags::MaskShift),
            shift_before,
            "the interrupted Shift modifier was released"
        );
        live.assert_frontmost_and_pointer_unchanged();
        live.turn(false);
        live.turn(true);
        assert!(
            live.act_with_fallback(click_target(target), false, true)
                .unwrap()
                .foreground_used
        );
        live.assert_frontmost_and_pointer_unchanged();
        live.turn(false);
    });
}

#[test]
#[ignore]
fn a_window_moved_during_the_foreground_idle_wait_is_stale() {
    run_live_test(|| {
        use objc2_core_graphics::{CGEventTapLocation, CGMouseButton};
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let action = click_target(live.point_inside(&obs, "canvas"));
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        let pointer = CGEvent::location(Some(&CGEvent::new(None).unwrap()));
        let movement = CGEvent::new_mouse_event(
            Some(&source),
            CGEventType::MouseMoved,
            pointer,
            CGMouseButton::Left,
        )
        .unwrap();
        let post = || {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&movement));
            record_test_hid_event();
        };
        post();
        thread::sleep(Duration::from_millis(20));
        let result = thread::scope(|scope| {
            let driver = &live;
            let pending = scope.spawn(move || driver.act_with_fallback(action, false, true));
            // Keep the driver in the idle wait while moving its observed window.
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(1) {
                post();
                thread::sleep(Duration::from_millis(50));
            }
            let output = Command::new("osascript").args(["-e", "tell application \"System Events\" to tell process \"CUFixture\" to set position of window 1 to {250, 250}"]).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            post();
            pending.join().unwrap()
        });
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Stale);
        assert_eq!(
            error.message,
            "the window changed since the last observe; observe again"
        );
        live.assert_frontmost_and_pointer_unchanged();
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("canvas_click")
        );
        let fresh = live.observe("dev.orca.cu-fixture", None);
        assert!(
            live.act_with_fallback(
                click_target(live.point_inside(&fresh, "canvas")),
                false,
                true
            )
            .unwrap()
            .foreground_used
        );
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
fn a_driver_hid_post_is_not_user_interference() {
    let state = GuiFailureKind::StateMismatch;
    assert!(!failure_is_inconclusive(state, 10.0, 2.0, None, Some(8.0)));
    assert!(!failure_is_inconclusive(
        state,
        10.0,
        2.0,
        Some(7.0),
        Some(8.0)
    ));
    assert!(!failure_is_inconclusive(
        state,
        10.0,
        2.0,
        Some(8.0),
        Some(7.0)
    ));
    assert!(failure_is_inconclusive(state, 10.0, 1.0, None, Some(8.0)));
    assert!(!failure_is_inconclusive(
        GuiFailureKind::NativeOperation,
        10.0,
        1.0,
        None,
        Some(8.0)
    ));
}

fn title_bar_button_frames(pid: i32, observation: &Observation) -> Vec<[i32; 4]> {
    use objc2_application_services::{AXUIElement, AXValue, AXValueType};
    use objc2_core_foundation::{CFString, CGPoint, CGSize};
    fn frame(element: &AXUIElement) -> [f64; 4] {
        let mut point = CGPoint::default();
        let mut size = CGSize::default();
        for (name, kind, pointer) in [
            (
                "AXPosition",
                AXValueType::CGPoint,
                std::ptr::NonNull::from(&mut point).cast(),
            ),
            (
                "AXSize",
                AXValueType::CGSize,
                std::ptr::NonNull::from(&mut size).cast(),
            ),
        ] {
            let value = fixture_ax_value(element, name)
                .unwrap()
                .downcast::<AXValue>()
                .unwrap();
            assert!(unsafe { value.value(kind, pointer) });
        }
        [point.x, point.y, size.width, size.height]
    }
    let app = unsafe { AXUIElement::new_application(pid) };
    let window = fixture_ax_value(&app, "AXMainWindow")
        .unwrap()
        .downcast::<AXUIElement>()
        .unwrap();
    let [x, y, width, height] = frame(&window);
    let screenshot = observation.screenshot.as_ref().unwrap();
    let geometry = orca_computer_use::geometry::CaptureGeometry {
        window_origin: (x, y),
        window_size: (width, height),
        image_size: (screenshot.width, screenshot.height),
    };
    let mut frames = vec![];
    for name in [
        "AXCloseButton",
        "AXMinimizeButton",
        "AXZoomButton",
        "AXFullScreenButton",
    ] {
        let Some(value) = fixture_ax_value(&window, name) else {
            continue;
        };
        let button = value.downcast::<AXUIElement>().unwrap();
        let subrole = fixture_ax_value(&button, "AXSubrole")
            .unwrap()
            .downcast::<CFString>()
            .unwrap()
            .to_string();
        assert!(matches!(
            subrole.as_str(),
            "AXCloseButton" | "AXMinimizeButton" | "AXZoomButton" | "AXFullScreenButton"
        ));
        frames.push(geometry.to_image(frame(&button)));
    }
    assert!(
        !frames.is_empty(),
        "positive control: native window frame buttons are identifiable by subrole"
    );
    frames
}

fn fixture_ax_value(
    element: &objc2_application_services::AXUIElement,
    name: &str,
) -> Option<objc2_core_foundation::CFRetained<objc2_core_foundation::CFType>> {
    use objc2_application_services::AXError;
    use objc2_core_foundation::CFString;
    use std::ptr::NonNull;
    assert_eq!(
        unsafe { element.set_messaging_timeout(2.0) },
        AXError::Success
    );
    let mut value = std::ptr::null();
    let result = unsafe {
        element.copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value))
    };
    if result == AXError::NoValue || result == AXError::AttributeUnsupported {
        return None;
    }
    assert_eq!(result, AXError::Success, "read fixture {name}");
    NonNull::new(value.cast_mut())
        .map(|pointer| unsafe { objc2_core_foundation::CFRetained::from_raw(pointer) })
}

fn fixture_screen_point(
    pid: i32,
    observation: &Observation,
    name: &str,
) -> objc2_core_foundation::CGPoint {
    use objc2_application_services::{AXUIElement, AXValue, AXValueType};
    use objc2_core_foundation::{CGPoint, CGSize};
    let app = unsafe { AXUIElement::new_application(pid) };
    let window = fixture_ax_value(&app, "AXMainWindow")
        .unwrap()
        .downcast::<AXUIElement>()
        .unwrap();
    let mut origin = CGPoint::default();
    let mut size = CGSize::default();
    let position = fixture_ax_value(&window, "AXPosition")
        .unwrap()
        .downcast::<AXValue>()
        .unwrap();
    let extent = fixture_ax_value(&window, "AXSize")
        .unwrap()
        .downcast::<AXValue>()
        .unwrap();
    assert!(unsafe {
        position.value(
            AXValueType::CGPoint,
            std::ptr::NonNull::from(&mut origin).cast(),
        )
    });
    assert!(unsafe {
        extent.value(
            AXValueType::CGSize,
            std::ptr::NonNull::from(&mut size).cast(),
        )
    });
    let image = observation.screenshot.as_ref().unwrap();
    let image_point = if name == "canvas" {
        [f64::from(image.width) * 0.75, f64::from(image.height) * 0.5]
    } else {
        let frame = observation
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some(name))
            .unwrap()
            .frame;
        [
            f64::from(frame[0]) + f64::from(frame[2]) / 2.0,
            f64::from(frame[1]) + f64::from(frame[3]) / 2.0,
        ]
    };
    let geometry = orca_computer_use::geometry::CaptureGeometry {
        window_origin: (origin.x, origin.y),
        window_size: (size.width, size.height),
        image_size: (image.width, image.height),
    };
    let (x, y) = geometry.to_screen(image_point).unwrap();
    CGPoint::new(x, y)
}

#[test]
#[ignore]
fn held_input_is_released_through_hid_after_finder_becomes_frontmost() {
    run_live_test(|| {
        use objc2_core_graphics::{CGEventFlags, CGMouseButton};
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live.launch_fixture();
        live.make_finder_frontmost();
        let original = live.background_state.unwrap();
        let obs = live.observe("dev.orca.cu-fixture", None);
        let point = fixture_screen_point(pid, &obs, "Increment");
        live.fixture_frontmost(pid);
        let modifiers = CGEventFlags::MaskShift
            | CGEventFlags::MaskCommand
            | CGEventFlags::MaskAlternate
            | CGEventFlags::MaskControl;
        assert!(
            (CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState) & modifiers)
                .is_empty()
        );
        let before = orca_computer_use::driver::macos::last_test_process_hid_post();
        let mut last_down = None;
        let _recovery = HeldInputRecovery;
        orca_computer_use::driver::macos::with_held_test_input(pid, point, || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !(CGEventSource::button_state(
                CGEventSourceStateID::HIDSystemState,
                CGMouseButton::Left,
            ) && CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState)
                .contains(CGEventFlags::MaskShift))
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                CGEventSource::button_state(
                    CGEventSourceStateID::HIDSystemState,
                    CGMouseButton::Left
                ),
                "positive control: the HID mouse button is held"
            );
            assert!(
                CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState)
                    .contains(CGEventFlags::MaskShift),
                "positive control: HID Shift is held"
            );
            last_down = orca_computer_use::driver::macos::last_test_process_hid_post();
            assert!(
                last_down > before,
                "driver action posts are recorded immediately"
            );
            live.make_finder_frontmost();
        })
        .unwrap();
        assert!(
            (CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState) & modifiers)
                .is_empty(),
            "HID modifiers were released after frontmost changed"
        );
        for button in [
            CGMouseButton::Left,
            CGMouseButton::Right,
            CGMouseButton::Center,
        ] {
            assert!(
                !CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, button),
                "HID mouse button remained down: {button:?}"
            );
        }
        assert!(
            orca_computer_use::driver::macos::last_test_process_hid_post() > last_down,
            "cleanup HID posts are recorded separately"
        );
        record_restoration(Some(original.0), original.1);
        live.background_state = Some(original);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

fn release_test_held_input() {
    use objc2_core_graphics::{CGEventField, CGEventFlags, CGEventTapLocation, CGMouseButton};
    let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
    let pointer = CGEvent::location(Some(&CGEvent::new(None).unwrap()));
    let mouse = CGEvent::new_mouse_event(
        Some(&source),
        CGEventType::LeftMouseUp,
        pointer,
        CGMouseButton::Left,
    )
    .unwrap();
    let shift = CGEvent::new_keyboard_event(Some(&source), 56, false).unwrap();
    for event in [&mouse, &shift] {
        CGEvent::set_flags(Some(event), CGEventFlags::empty());
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::EventSourceUserData,
            0x4F52_4341,
        );
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
        record_test_hid_event();
    }
    thread::sleep(Duration::from_millis(30));
}

struct HeldInputRecovery;
impl Drop for HeldInputRecovery {
    fn drop(&mut self) {
        release_test_held_input();
    }
}

#[test]
#[ignore]
fn foreground_password_focus_requires_approval_after_the_click() {
    password_focus_after_click(false);
}
#[test]
#[ignore]
fn foreground_text_key_checks_password_focus_after_the_click() {
    password_focus_after_click(true);
}
fn password_focus_after_click(key: bool) {
    run_live_test(|| {
        use objc2_application_services::AXUIElement;
        use objc2_core_foundation::CFString;
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture_with_flags(&["--foreground-password-probe"]);
        live.assert_log_contains(&log, r#""type":"focus_reset""#);
        live.fixture_frontmost(pid);
        live.background_state = Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
        let obs = live.observe("dev.orca.cu-fixture", None);
        let app = unsafe { AXUIElement::new_application(pid) };
        assert!(
            fixture_ax_value(&app, "AXFocusedUIElement").is_none(),
            "the probe starts with no focused element"
        );
        let target = live.point_inside(&obs, "Password");
        // Read the real secure field from the tree, bypassing only the probe's
        // non-focusable hit-test behavior; foreground HID still clicks the real view.
        let window = fixture_ax_value(&app, "AXMainWindow")
            .unwrap()
            .downcast::<AXUIElement>()
            .unwrap();

        // Read the password field through the AX tree, not an intercepted hit.
        fn find_secure(
            element: &AXUIElement,
        ) -> Option<objc2_core_foundation::CFRetained<AXUIElement>> {
            native_ax_nodes(element).into_iter().find(|node| {
                native_ax_string(node, "AXSubrole").as_deref() == Some("AXSecureTextField")
            })
        }
        let secure = find_secure(&window).expect("real password field exists in the fixture");
        let length = || {
            fixture_ax_value(&secure, "AXValue")
                .unwrap()
                .downcast::<CFString>()
                .unwrap()
                .to_string()
                .chars()
                .count()
        };
        assert_eq!(length(), 0);
        let action = if key {
            Action::Key {
                keys: vec!["x".into()],
                target: Some(target),
            }
        } else {
            Action::Type {
                text: "ok".into(),
                target: Some(target),
            }
        };
        let error = live
            .act_with_fallback(action.clone(), false, true)
            .unwrap_err();
        assert_eq!(
            length(),
            0,
            "unapproved foreground text did not enter the password field"
        );
        assert_eq!(error.kind, ErrorKind::NeedsSecureApproval, "{error:?}");
        live.assert_frontmost_and_pointer_unchanged();
        // Reset through the fixture's real AXPress action, so the approved
        // control also starts without focus and must use the same fallback.
        let fresh = live.observe("dev.orca.cu-fixture", None);
        live.act(
            click_target(element_named(&fresh, "Reset probe focus")),
            false,
        )
        .unwrap();
        assert!(fixture_ax_value(&app, "AXFocusedUIElement").is_none());
        // The approved control uses the same fallback click on the real field.
        let outcome = live.act_with_fallback(action, true, true).unwrap();
        assert!(outcome.foreground_used);
        assert_eq!(
            length(),
            if key { 1 } else { 2 },
            "approved foreground text reached the password field"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn an_observed_text_area_with_a_failed_description_can_be_targeted_by_id() {
    run_live_test(|| {
        use objc2_application_services::{AXError, AXUIElement};
        use objc2_core_foundation::CFString;
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .next()
            .unwrap();
        let app = unsafe { AXUIElement::new_application(pid) };
        let window = fixture_ax_value(&app, "AXMainWindow")
            .unwrap()
            .downcast::<AXUIElement>()
            .unwrap();
        let text = fixture_ax_role(&window, "AXTextArea").expect("fixture text view");
        let mut value = std::ptr::null();
        let description = unsafe {
            text.copy_attribute_value(
                &CFString::from_str("AXDescription"),
                std::ptr::NonNull::from(&mut value),
            )
        };
        assert_eq!(
            description,
            AXError::Failure,
            "the regression must exercise a real failing optional description read"
        );
        assert!(value.is_null());
        // Same scroll view, with an available label, is the positive control.
        let control = live
            .act(
                Action::Scroll {
                    target: element_named(&obs, "List"),
                    direction: Direction::Down,
                    amount: 1,
                },
                false,
            )
            .unwrap();
        let control_position = control.scroll_position.unwrap();
        assert!(control_position > 0.0);
        live.assert_log_contains(&log, r#""type":"scroll""#);
        let fresh = live.observe("dev.orca.cu-fixture", None);
        let element = fresh
            .elements
            .iter()
            .find(|e| e.role == "text area")
            .expect("the failing-description element is observable");
        assert!(element.label.is_none());
        let outcome = live
            .act(
                Action::Scroll {
                    target: Target::Element {
                        element: element.id,
                    },
                    direction: Direction::Down,
                    amount: 1,
                },
                false,
            )
            .unwrap();
        assert!(!outcome.foreground_used);
        assert!(outcome.scroll_position.unwrap() > control_position);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn an_action_after_the_element_label_changed_is_stale() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (_, log) = live.launch_fixture_with_flags(&["--rename-button-after-press"]);
        live.make_finder_frontmost();
        let obs = live.observe("dev.orca.cu-fixture", None);
        let action = click_target(element_named(&obs, "Increment"));
        // Keep the observed table while the real app renames the pressed button.
        let request = Request::Act(ActParams {
            app: "dev.orca.cu-fixture".into(),
            window: Some(obs.window.id),
            action: action.clone(),
            foreground_fallback: false,
            observe_after: false,
            include_screenshot: false,
            secure_approved: false,
        });
        assert!(matches!(
            live.client
                .call(request.clone(), Duration::from_secs(30))
                .unwrap(),
            Reply::Acted { .. }
        ));
        live.assert_log_contains(&log, r#""count":1"#);
        let error = live
            .client
            .call(request, Duration::from_secs(30))
            .unwrap_err();
        assert!(
            matches!(
                error,
                orca_computer_use::client::CallError::Driver(DriverError {
                    kind: ErrorKind::Stale,
                    ..
                })
            ),
            "changed labels require a fresh observation: {error:?}"
        );
        let fresh = live.observe("dev.orca.cu-fixture", None);
        live.act(click_target(element_named(&fresh, "Renamed")), false)
            .unwrap();
        live.assert_log_contains(&log, r#""count":2"#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

fn fixture_ax_role(
    element: &objc2_application_services::AXUIElement,
    role: &str,
) -> Option<objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>> {
    native_ax_nodes(element)
        .into_iter()
        .find(|node| native_ax_string(node, "AXRole").as_deref() == Some(role))
}

fn pointer_restored(
    expected: objc2_core_foundation::CGPoint,
    actual: objc2_core_foundation::CGPoint,
) -> bool {
    // CGWarpMouseCursorPosition rounds subpixel trackpad positions to points.
    // Use the same precision as record_restoration and the production restore.
    (expected.x - actual.x).abs() <= 1.0 && (expected.y - actual.y).abs() <= 1.0
}

#[test]
fn warp_precision_accepts_rounding_but_rejects_a_moved_pointer() {
    use objc2_core_foundation::CGPoint;
    let expected = CGPoint::new(1439.8359375, 992.703125);
    assert!(pointer_restored(expected, CGPoint::new(1439.0, 992.0)));
    assert!(!pointer_restored(expected, CGPoint::new(1438.0, 992.0)));
    assert!(!pointer_restored(expected, CGPoint::new(1439.0, 990.0)));
}

fn fixture_ax_named(
    element: &objc2_application_services::AXUIElement,
    name: &str,
) -> Option<objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>> {
    native_ax_nodes(element)
        .into_iter()
        .find(|node| native_ax_string(node, "AXDescription").as_deref() == Some(name))
}

fn password_length(pid: i32) -> usize {
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::CFString;
    let app = unsafe { AXUIElement::new_application(pid) };
    let field = fixture_ax_named(&app, "Password").expect("password field");
    fixture_ax_value(&field, "AXValue")
        .unwrap()
        .downcast::<CFString>()
        .unwrap()
        .to_string()
        .chars()
        .count()
}

// Copy all data types into independent items before replacing the clipboard.
struct TestClipboard(Vec<objc2::rc::Retained<objc2_app_kit::NSPasteboardItem>>);
impl TestClipboard {
    fn text(text: &str) -> Self {
        use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString};
        let board = NSPasteboard::generalPasteboard();
        let mut saved = vec![];
        if let Some(items) = board.pasteboardItems() {
            for item in items.iter() {
                let copy = NSPasteboardItem::new();
                for kind in item.types().iter() {
                    let data = item.dataForType(&kind).expect("snapshot clipboard data");
                    assert!(copy.setData_forType(&data, &kind));
                }
                saved.push(copy);
            }
        }
        let guard = Self(saved);
        board.clearContents();
        assert!(
            board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
        );
        guard
    }
}
impl Drop for TestClipboard {
    fn drop(&mut self) {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        let objects: Vec<_> = self
            .0
            .iter()
            .map(|item| ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&**item))
            .collect();
        if !objects.is_empty() {
            assert!(board.writeObjects(&objc2_foundation::NSArray::from_slice(&objects)));
        }
    }
}

#[test]
#[ignore]
fn text_keys_into_password_require_approval() {
    run_live_test(|| {
        let Some((live, _, obs)) = background_fixture() else {
            return;
        };
        let pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .next()
            .unwrap();
        let password = element_named(&obs, "Password");
        let action = Action::Key {
            keys: vec!["x".into()],
            target: Some(password),
        };
        let result = live.act(action.clone(), false);
        assert_eq!(password_length(pid), 0, "unapproved x entered password");
        assert_eq!(result.unwrap_err().kind, ErrorKind::NeedsSecureApproval);
        assert!(!live.act(action, true).unwrap().foreground_used);
        assert_eq!(password_length(pid), 1, "approved x positive control");
        live.act(
            Action::Key {
                keys: vec!["tab".into()],
                target: Some(password),
            },
            false,
        )
        .unwrap();
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn paste_into_focused_password_requires_approval() {
    run_live_test(|| {
        let Some((live, _, obs)) = background_fixture() else {
            return;
        };
        let pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .next()
            .unwrap();
        let _clipboard = TestClipboard::text("paste");
        live.act(
            Action::Type {
                text: "".into(),
                target: Some(element_named(&obs, "Password")),
            },
            true,
        )
        .unwrap();
        let action = Action::Key {
            keys: vec!["cmd".into(), "v".into()],
            target: None,
        };
        let result = live.act_with_fallback(action.clone(), false, true);
        assert_eq!(password_length(pid), 0, "unapproved paste entered password");
        assert_eq!(result.unwrap_err().kind, ErrorKind::NeedsSecureApproval);
        assert!(
            live.act_with_fallback(action, true, true)
                .unwrap()
                .foreground_used
        );
        assert_eq!(password_length(pid), 5, "approved paste positive control");
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn background_type_checks_password_focus_after_a_noop_focus_setter() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live.launch_fixture_with_flags(&["--sticky-password-focus"]);
        live.fixture_frontmost(pid);
        live.background_state = Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
        let obs = live.observe("dev.orca.cu-fixture", None);
        assert!(obs.focused_secure);
        let action = Action::Type {
            text: "ok".into(),
            target: Some(element_named(&obs, "Name")),
        };
        let result = live.act(action.clone(), false);
        assert_eq!(
            password_length(pid),
            0,
            "actual password recipient must remain empty"
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::NeedsSecureApproval);
        live.act(action, true).unwrap();
        assert_eq!(
            password_length(pid),
            2,
            "approved same-target positive control"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

fn restore_suite_if_live(live_ran: bool, restore: impl FnOnce()) -> bool {
    !live_ran || std::panic::catch_unwind(std::panic::AssertUnwindSafe(restore)).is_ok()
}
#[test]
fn a_suite_without_live_tests_does_not_restore_the_gui() {
    let restored = std::cell::Cell::new(0);
    assert!(restore_suite_if_live(false, || restored.set(restored.get() + 1)));
    assert_eq!(restored.get(), 0, "ordinary tests must leave the GUI alone");
    assert!(restore_suite_if_live(true, || restored.set(restored.get() + 1)));
    assert_eq!(restored.get(), 1, "live-suite restoration positive control");
}

#[test]
#[ignore]
fn act_and_observe_choose_the_first_window_when_main_is_absent() {
    run_live_test(|| {
        use objc2_application_services::AXUIElement;
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture_with_flags(&["--no-main-window"]);
        let application = unsafe { AXUIElement::new_application(pid) };
        assert!(fixture_ax_value(&application, "AXMainWindow").is_none());
        let obs = live.observe("dev.orca.cu-fixture", None);
        let outcome = live
            .act(click_target(element_named(&obs, "Increment")), false)
            .unwrap();
        assert_eq!(outcome.observation.unwrap().window.id, obs.window.id);
        live.assert_log_contains(&log, r#""count":1"#);
        let explicit = live
            .act_app(
                "dev.orca.cu-fixture",
                Some(obs.window.id),
                click_target(element_named(&obs, "Increment")),
                false,
                false,
            )
            .unwrap();
        assert_eq!(explicit.observation.unwrap().window.id, obs.window.id);
        live.assert_log_contains(&log, r#""count":2"#);
    });
}

#[test]
#[ignore]
fn approved_actions_and_observations_require_the_exact_bundle_id() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let result = live.client.call(
            Request::Observe(ObserveParams {
                app: "CUFixture".into(),
                window: None,
                query: None,
                max_elements: 300,
                include_screenshot: false,
            }),
            Duration::from_secs(30),
        );
        assert!(matches!(
            result,
            Err(orca_computer_use::client::CallError::Driver(DriverError {
                kind: ErrorKind::AppNotFound,
                ..
            }))
        ));
        assert_eq!(
            live.act_app(
                "CUFixture",
                None,
                click_target(element_named(&obs, "Increment")),
                false,
                false
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppNotFound
        );
        live.act(click_target(element_named(&obs, "Increment")), false)
            .unwrap();
        live.assert_log_contains(&log, r#""count":1"#);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn closing_a_window_preserves_the_successful_action_outcome() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        live.act(click_target(element_named(&obs, "Increment")), false)
            .unwrap();
        live.assert_log_contains(&log, r#""count":1"#);
        let outcome = live
            .act(click_target(element_named(&obs, "Close window")), false)
            .unwrap();
        live.assert_log_contains(&log, r#""type":"closed""#);
        assert!(outcome.observation.is_none());
        assert!(
            outcome
                .note
                .unwrap()
                .contains("The window closed after the action; observe again.")
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn launching_a_cold_app_does_not_activate_it() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        if !bundle_pids("dev.orca.cu-fixture").is_empty() {
            eprintln!("skipped: fixture already running");
            return;
        }
        // Register the test bundle once, then stop only our process for a cold launch.
        let (pid, _) = live.launch_fixture();
        let old = NSRunningApplication::runningApplicationWithProcessIdentifier(pid).unwrap();
        assert!(old.terminate());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !old.isTerminated() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        live.make_finder_frontmost();
        let before = bundle_pids("dev.orca.cu-fixture");
        let result = live.client.call(
            Request::LaunchApp {
                app: "dev.orca.cu-fixture".into(),
                open: vec![],
            },
            Duration::from_secs(60),
        );
        register_launch_pids(
            &mut live.launched,
            &before,
            &bundle_pids("dev.orca.cu-fixture"),
        );
        let Reply::Launched { app } = result.unwrap() else {
            panic!("launch reply");
        };
        live.assert_frontmost_and_pointer_unchanged();
        assert!(
            app.pid.is_some() && !app.windows.is_empty(),
            "cold launch readiness"
        );
        let obs = live.observe("dev.orca.cu-fixture", None);
        assert!(
            obs.elements
                .iter()
                .any(|e| e.label.as_deref() == Some("Increment")),
            "ready window positive control"
        );
    });
}

fn set_fixture_ax_bool(element: &objc2_application_services::AXUIElement, name: &str, value: bool) {
    use objc2_application_services::AXError;
    use objc2_core_foundation::{CFBoolean, CFString};
    assert_eq!(
        unsafe {
            element.set_attribute_value(&CFString::from_str(name), CFBoolean::new(value).as_ref())
        },
        AXError::Success
    );
}
fn fixture_window(
    pid: i32,
) -> objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement> {
    let application = unsafe { objc2_application_services::AXUIElement::new_application(pid) };
    fixture_ax_value(&application, "AXMainWindow")
        .unwrap()
        .downcast()
        .unwrap()
}
#[test]
#[ignore]
fn an_offscreen_row_is_scrolled_in_or_refused_before_hid_clicking() {
    run_live_test(|| {
        use orca_computer_use::driver::macos::test_element_click_point;
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture_with_flags(&["--rows-probe"]);
        let obs = live.observe("dev.orca.cu-fixture", None);
        let row = obs
            .elements
            .iter()
            .find(|e| e.label.as_deref().is_some_and(|s| s.starts_with("Row ")) && e.frame[3] == 0)
            .expect("offscreen row");
        let window = fixture_window(pid);
        let native_row = fixture_ax_named(&window, row.label.as_ref().unwrap()).unwrap();
        let point = test_element_click_point(native_row, window.clone());
        if let Ok(point) = point {
            // Read-only RED: the old native frame center was outside the window.
            let system = unsafe { objc2_application_services::AXUIElement::new_system_wide() };
            let mut hit = std::ptr::null();
            assert_eq!(
                unsafe {
                    system.copy_element_at_position(
                        point.x as f32,
                        point.y as f32,
                        std::ptr::NonNull::from(&mut hit),
                    )
                },
                objc2_application_services::AXError::Success,
                "native point must be hittable inside the fixture"
            );
            let hit = unsafe {
                objc2_core_foundation::CFRetained::from_raw(
                    std::ptr::NonNull::new(hit.cast_mut()).unwrap(),
                )
            };
            let mut owner = 0;
            assert_eq!(
                unsafe { hit.pid(std::ptr::NonNull::from(&mut owner)) },
                objc2_application_services::AXError::Success
            );
            assert_eq!(
                owner, pid,
                "offscreen native center would click through to another app"
            );
        } else {
            assert_eq!(
                point.unwrap_err().message,
                "The element is outside the visible window; observe again"
            );
        }
        let before = orca_computer_use::driver::macos::last_test_process_hid_post();
        let outcome = live.act_with_fallback(
            Action::Click {
                target: Target::Element { element: row.id },
                button: Button::Left,
                count: 1,
                modifiers: vec!["shift".into()],
            },
            false,
            true,
        );
        match outcome {
            Ok(outcome) => {
                assert!(outcome.foreground_used);
                live.assert_log_contains(&log, r#""count":1"#);
            }
            Err(failure) => {
                assert_eq!(
                    failure.message,
                    "The element is outside the visible window; observe again"
                );
                assert_eq!(
                    orca_computer_use::driver::macos::last_test_process_hid_post(),
                    before
                );
            }
        }
        let fresh = live.observe("dev.orca.cu-fixture", None);
        live.act_with_fallback(
            click_target(live.point_inside(&fresh, "canvas")),
            false,
            true,
        )
        .unwrap();
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
    });
}
#[test]
#[ignore]
fn a_finder_window_covering_the_canvas_is_refused_without_posting() {
    run_live_test(|| {
        use orca_computer_use::driver::macos::test_check_click_point;
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture();
        let obs = live.observe("dev.orca.cu-fixture", None);
        let point = fixture_screen_point(pid, &obs, "canvas");
        let output = Command::new("osascript")
            .args([
                "-e",
                "tell application \"Finder\"
set probe to make new Finder window
set bounds of probe to {100, 100, 1100, 1000}
activate
return id of probe
end tell",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let finder = FinderWindow(
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        );
        live.make_finder_frontmost();
        // Probe is read-only; RED never sends a click into Finder or the host.
        assert_eq!(
            test_check_click_point(pid, point).unwrap_err().message,
            "Another window covers the target point"
        );
        let before = orca_computer_use::driver::macos::last_test_process_hid_post();
        let failure =
            orca_computer_use::driver::macos::test_click_at_point(pid, point).unwrap_err();
        assert_eq!(failure.message, "Another window covers the target point");
        assert_eq!(
            orca_computer_use::driver::macos::last_test_process_hid_post(),
            before,
            "Finder received no marked event"
        );
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("canvas_click")
        );
        live.assert_frontmost_and_pointer_unchanged();
        drop(finder);
        live.fixture_frontmost(pid);
        live.act_with_fallback(click_target(live.point_inside(&obs, "canvas")), false, true)
            .unwrap();
        live.assert_log_contains(&log, r#""type":"canvas_click""#);
    });
}
#[test]
#[ignore]
fn a_minimized_window_is_clicked_then_minimized_again() {
    run_live_test(|| {
        let Some((live, log, obs)) = background_fixture() else {
            return;
        };
        let pid = bundle_pids("dev.orca.cu-fixture")
            .into_iter()
            .next()
            .unwrap();
        let window = fixture_window(pid);
        live.act_with_fallback(click_target(live.point_inside(&obs, "canvas")), false, true)
            .unwrap();
        set_fixture_ax_bool(&window, "AXMinimized", true);
        thread::sleep(Duration::from_millis(250));
        assert!(
            fixture_ax_value(&window, "AXMinimized")
                .unwrap()
                .downcast::<objc2_core_foundation::CFBoolean>()
                .unwrap()
                .value()
        );
        let outcome = live
            .act_with_fallback(click_target(live.point_inside(&obs, "canvas")), false, true)
            .unwrap();
        assert!(outcome.foreground_used);
        assert!(
            fixture_ax_value(&window, "AXMinimized")
                .unwrap()
                .downcast::<objc2_core_foundation::CFBoolean>()
                .unwrap()
                .value()
        );
        let clicks = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|s| s.contains("canvas_click"))
            .count();
        assert_eq!(clicks, 2);
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn a_foreground_scroll_moves_the_target_list_and_restores_the_pointer() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture_with_flags(&["--foreground-scroll-probe"]);
        let obs = live.observe("dev.orca.cu-fixture", None);
        let away = fixture_screen_point(pid, &obs, "Name");
        assert_eq!(
            CGWarpMouseCursorPosition(away),
            objc2_core_graphics::CGError::Success
        );
        live.background_state = Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
        let outcome = live
            .act_with_fallback(
                Action::Scroll {
                    target: element_named(&obs, "List"),
                    direction: Direction::Down,
                    amount: 5,
                },
                false,
                true,
            )
            .unwrap();
        assert!(
            outcome.foreground_used,
            "fixture has no background scroll bar"
        );
        live.assert_log_contains(&log, r#""type":"scroll""#);
        let events: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "scroll" && e["y"].as_f64().is_some_and(|y| y > 0.0)),
            "foreground wheel must move the list while the pointer started away"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn background_type_with_a_target_inserts_after_existing_text() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (_, log) = live.launch_fixture_with_flags(&["--prefilled-name"]);
        live.make_finder_frontmost();
        let obs = live.observe("dev.orca.cu-fixture", None);
        let target = element_named(&obs, "Name");
        assert_eq!(live.fixture_control_value("Name"), "a🦀c");
        for (text, expected) in [("x", "a🦀cx"), ("y", "a🦀cxy")] {
            let outcome = live
                .act(
                    Action::Type {
                        text: text.into(),
                        target: Some(target),
                    },
                    false,
                )
                .unwrap();
            assert!(!outcome.foreground_used);
            let obs = outcome.observation.unwrap();
            let field = obs
                .elements
                .iter()
                .find(|e| e.label.as_deref() == Some("Name"))
                .unwrap();
            assert!(!field.secure);
            assert_eq!(field.value.as_deref(), Some(expected));
            assert_eq!(live.fixture_control_value("Name"), expected);
            live.assert_log_contains(&log, &format!("\"value\":\"{expected}\""));
        }
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn unreported_password_focus_refuses_unapproved_text_keys() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live
            .launch_fixture_with_flags(&["--foreground-password-probe", "--hide-focused-element"]);
        live.fixture_frontmost(pid);
        live.background_state = Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
        let obs = live.observe("dev.orca.cu-fixture", None);
        let action = Action::Key {
            keys: vec!["x".into()],
            target: Some(live.point_inside(&obs, "Password")),
        };
        let result = live.act_with_fallback(action.clone(), false, true);
        assert_eq!(
            password_length(pid),
            0,
            "missing AX focus must fail closed before text delivery"
        );
        let failure = result.unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Failed);
        assert!(failure.message.contains("no focused element"));
        let outcome = live.act_with_fallback(action, true, true).unwrap();
        assert!(outcome.foreground_used);
        assert_eq!(
            password_length(pid),
            1,
            "approved same-hidden-focus positive control"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn foreground_type_at_a_point_keeps_the_clicked_caret() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) =
            live.launch_fixture_with_flags(&["--prefilled-name", "--foreground-type-probe"]);
        live.fixture_frontmost(pid);
        live.background_state = Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
        let obs = live.observe("dev.orca.cu-fixture", None);
        let frame = obs
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Name"))
            .unwrap()
            .frame;
        let target = Target::Point {
            point: [
                f64::from(frame[0]) + 24.0,
                f64::from(frame[1]) + f64::from(frame[3]) / 2.0,
            ],
        };
        let outcome = live
            .act_with_fallback(
                Action::Type {
                    text: "x".into(),
                    target: Some(target),
                },
                false,
                true,
            )
            .unwrap();
        assert!(outcome.foreground_used);
        let events: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let click = events
            .iter()
            .find(|e| e["type"] == "name_click")
            .expect("native caret receipt");
        eprintln!("native click receipt: {click}");
        assert_eq!(click["length"], 0);
        let expected = match click["location"].as_u64().unwrap() {
            1 => "ax🦀c",
            3 => "a🦀xc",
            other => panic!("click must put the caret in the middle of a🦀c, got {other}"),
        };
        assert_eq!(
            events.iter().rev().find(|e| e["type"] == "input").unwrap()["value"],
            expected,
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn foreground_type_with_an_element_target_inserts_after_existing_text() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture_with_flags(&[
            "--prefilled-name",
            "--foreground-type-probe",
            "--select-name-on-click",
        ]);
        live.fixture_frontmost(pid);
        live.background_state = Some((pid, CGEvent::location(Some(&CGEvent::new(None).unwrap()))));
        let obs = live.observe("dev.orca.cu-fixture", None);
        let outcome = live
            .act_with_fallback(
                Action::Type {
                    text: "x".into(),
                    target: Some(element_named(&obs, "Name")),
                },
                false,
                true,
            )
            .unwrap();
        assert!(outcome.foreground_used);
        live.assert_log_contains(&log, "\"value\":\"a🦀cx\"");
        live.assert_frontmost_and_pointer_unchanged();
    });
}

fn type_with_optional_selection(flags: &[&str], expected: &str) {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let mut options = vec!["--prefilled-name"];
        options.extend_from_slice(flags);
        let (_, log) = live.launch_fixture_with_flags(&options);
        live.make_finder_frontmost();
        let obs = live.observe("dev.orca.cu-fixture", None);
        // Same-field positive control: text delivery itself works without the optional collapse.
        let control = live
            .act(
                Action::Type {
                    text: "u".into(),
                    target: None,
                },
                false,
            )
            .unwrap();
        assert!(!control.foreground_used);
        live.assert_log_contains(&log, "\"value\":\"a🦀cu\"");
        if flags.contains(&"--expire-selection-focus") || flags.contains(&"--no-name-length") {
            let app = unsafe {
                objc2_application_services::AXUIElement::new_application(
                    live.apps()
                        .into_iter()
                        .find(|a| a.bundle_id == "dev.orca.cu-fixture")
                        .unwrap()
                        .pid
                        .unwrap(),
                )
            };
            let focused = fixture_ax_value(&app, "AXFocusedUIElement")
                .unwrap()
                .downcast::<objc2_application_services::AXUIElement>()
                .unwrap();
            assert_eq!(
                fixture_ax_value(&focused, "AXIdentifier")
                    .unwrap()
                    .downcast::<objc2_core_foundation::CFString>()
                    .unwrap()
                    .to_string(),
                "expiring-name-focus"
            );
            if flags.contains(&"--no-name-length") {
                assert!(
                    fixture_ax_value(&focused, "AXNumberOfCharacters").is_none(),
                    "the native length read must be unavailable"
                );
            }
        }
        let result = live
            .act(
                Action::Type {
                    text: "x".into(),
                    target: Some(element_named(&obs, "Name")),
                },
                false,
            )
            .unwrap();
        assert!(!result.foreground_used);
        live.assert_log_contains(&log, &format!("\"value\":\"{expected}\""));
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn targeted_type_without_ax_value_uses_the_character_count() {
    type_with_optional_selection(&["--no-name-value"], "a🦀cux");
}

#[test]
#[ignore]
fn targeted_type_without_a_readable_length_still_delivers_text() {
    type_with_optional_selection(&["--no-name-value", "--no-name-length"], "a🦀cux");
}

#[test]
#[ignore]
fn targeted_type_with_an_unwritable_selection_still_delivers_text() {
    type_with_optional_selection(&["--expire-selection-focus"], "a🦀cux");
}

fn foreground_point_click_without_early_hit(variant: &str) {
    run_live_test(|| {
        use objc2_application_services::{AXError, AXUIElement};
        use objc2_core_foundation::CFString;
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture_with_flags(&["--reject-hit-before-raise"]);
        let obs = live.observe("dev.orca.cu-fixture", None);
        let target = live.point_inside(
            &obs,
            if variant == "hidden" {
                "Increment"
            } else {
                "canvas"
            },
        );
        let action = Action::Click {
            target,
            button: if variant == "right" {
                Button::Right
            } else {
                Button::Left
            },
            count: if variant == "double" { 2 } else { 1 },
            modifiers: if variant == "modified" {
                vec!["shift".into()]
            } else {
                vec![]
            },
        };
        let app = unsafe { AXUIElement::new_application(pid) };
        let window = fixture_ax_value(&app, "AXMainWindow")
            .unwrap()
            .downcast::<AXUIElement>()
            .unwrap();
        assert_eq!(
            unsafe { window.perform_action(&CFString::from_str("AXRaise")) },
            AXError::Success
        );
        live.make_finder_frontmost();
        // Same native target/action control with a readable hit test, before resetting the probe.
        live.act_with_fallback(action.clone(), false, true).unwrap();
        let obs = live.observe("dev.orca.cu-fixture", None);
        live.act(
            click_target(element_named(&obs, "Reset probe focus")),
            false,
        )
        .unwrap();
        live.assert_log_contains(&log, "\"type\":\"focus_reset\"");
        if variant == "hidden" {
            let application =
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid).unwrap();
            let output = Command::new("osascript")
                .args(["-e", "tell application \"System Events\" to set visible of process \"CUFixture\" to false"])
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while !application.isHidden() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(20));
            }
        }
        let outcome = live.act_with_fallback(action, false, true).unwrap();
        assert!(outcome.foreground_used, "{variant}");
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("\"type\":\"early_hit\""),
            "{variant}: must not hit test before raising the fixture"
        );
        live.assert_frontmost_and_pointer_unchanged();
    });
}

#[test]
#[ignore]
fn foreground_point_click_double_skips_the_pre_activation_hit_test() {
    foreground_point_click_without_early_hit("double");
}

#[test]
#[ignore]
fn foreground_point_click_right_skips_the_pre_activation_hit_test() {
    foreground_point_click_without_early_hit("right");
}

#[test]
#[ignore]
fn foreground_point_click_modified_skips_the_pre_activation_hit_test() {
    foreground_point_click_without_early_hit("modified");
}

#[test]
#[ignore]
fn foreground_point_click_hidden_skips_the_pre_activation_hit_test() {
    foreground_point_click_without_early_hit("hidden");
}

fn native_ax_optional_value(
    element: &objc2_application_services::AXUIElement,
    name: &str,
) -> Option<objc2_core_foundation::CFRetained<objc2_core_foundation::CFType>> {
    use objc2_application_services::AXError;
    use objc2_core_foundation::CFString;
    let mut value = std::ptr::null();
    let result = unsafe {
        element.copy_attribute_value(
            &CFString::from_str(name),
            std::ptr::NonNull::from(&mut value),
        )
    };
    if matches!(
        result,
        AXError::Failure | AXError::NoValue | AXError::AttributeUnsupported
    ) {
        return None;
    }
    assert_eq!(result, AXError::Success, "independent native read {name}");
    std::ptr::NonNull::new(value.cast_mut())
        .map(|p| unsafe { objc2_core_foundation::CFRetained::from_raw(p) })
}

// Native controls read only the test's own app/window, independently of Orca's tree.
fn native_ax_nodes(
    root: &objc2_application_services::AXUIElement,
) -> Vec<objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>> {
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::{CFArray, CFRetained, CFType};
    let mut nodes = vec![unsafe { CFRetained::retain(std::ptr::NonNull::from(root)) }];
    let mut index = 0;
    while index < nodes.len() && index < 5000 {
        if let Some(children) = native_ax_optional_value(&nodes[index], "AXChildren")
            && let Some(array) = children.downcast_ref::<CFArray>()
        {
            for i in 0..array.len() {
                let child = unsafe { &*array.value_at_index(i as isize).cast::<CFType>() };
                if let Some(child) = child.downcast_ref::<AXUIElement>()
                    && !nodes.iter().any(|seen| &**seen == child)
                {
                    nodes.push(unsafe { CFRetained::retain(std::ptr::NonNull::from(child)) });
                }
            }
        }
        index += 1;
    }
    nodes
}

fn native_ax_string(
    element: &objc2_application_services::AXUIElement,
    name: &str,
) -> Option<String> {
    use objc2_core_foundation::{CFNumber, CFString};
    native_ax_optional_value(element, name).and_then(|value| {
        value
            .downcast_ref::<CFString>()
            .map(ToString::to_string)
            .or_else(|| {
                value
                    .downcast_ref::<CFNumber>()
                    .and_then(|n| n.as_f64())
                    .map(|n| n.to_string())
            })
    })
}

fn launch_test_app(live: &mut Live, bundle: &str, open: Vec<String>) -> AppInfo {
    let before = bundle_pids(bundle);
    let result = live.client.call(
        Request::LaunchApp {
            app: bundle.into(),
            open,
        },
        Duration::from_secs(60),
    );
    register_launch_pids(&mut live.launched, &before, &bundle_pids(bundle));
    let Reply::Launched { app } = result.unwrap() else {
        panic!("expected launched {bundle}")
    };
    app
}

#[test]
#[ignore]
fn calculator_observe_survives_failed_subroles_and_digit_press_changes_display() {
    run_live_test(|| {
        use objc2_application_services::{AXError, AXUIElement};
        use objc2_core_foundation::CFString;
        let Some(mut live) = Live::start() else {
            return;
        };
        if !bundle_pids("com.apple.calculator").is_empty() {
            eprintln!("skipped: Calculator already running");
            return;
        }
        let app = launch_test_app(&mut live, "com.apple.calculator", vec![]);
        let application = unsafe { AXUIElement::new_application(app.pid.unwrap()) };
        live.fixture_frontmost(app.pid.unwrap());
        let native_window = fixture_ax_value(&application, "AXMainWindow")
            .unwrap()
            .downcast::<AXUIElement>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let nodes = loop {
            let nodes = native_ax_nodes(&native_window);
            if nodes
                .iter()
                .any(|node| native_ax_string(node, "AXRole").as_deref() == Some("AXButton"))
            {
                break nodes;
            }
            assert!(
                Instant::now() < deadline,
                "native Calculator controls not ready"
            );
            thread::sleep(Duration::from_millis(50));
        };
        assert!(
            nodes
                .iter()
                .any(|node| native_ax_string(node, "AXRole").as_deref() == Some("AXButton")),
            "native button positive control"
        );
        let has_failed_subrole = nodes.iter().any(|node| {
            let mut value = std::ptr::null();
            let result = unsafe {
                node.copy_attribute_value(
                    &CFString::from_str("AXSubrole"),
                    std::ptr::NonNull::from(&mut value),
                )
            };
            if let Some(pointer) = std::ptr::NonNull::new(value.cast_mut()) {
                drop(unsafe { objc2_core_foundation::CFRetained::from_raw(pointer) });
            }
            result == AXError::Failure
        });
        assert!(
            has_failed_subrole,
            "same-app native AXSubrole Failure control"
        );
        let clear = nodes
            .iter()
            .find(|node| {
                native_ax_string(node, "AXRole").as_deref() == Some("AXButton")
                    && ["AXTitle", "AXDescription"].iter().any(|name| {
                        native_ax_string(node, name).as_deref().is_some_and(|s| {
                            s.to_lowercase().contains("clear")
                                || s.contains("清除")
                                || s == "C"
                                || s == "AC"
                        })
                    })
            })
            .expect("native clear control");
        assert_eq!(
            unsafe { clear.perform_action(&CFString::from_str("AXPress")) },
            AXError::Success
        );
        thread::sleep(Duration::from_millis(150));
        let native_digit = nodes
            .iter()
            .find(|node| {
                native_ax_string(node, "AXRole").as_deref() == Some("AXButton")
                    && ["AXTitle", "AXDescription"].iter().any(|name| {
                        native_ax_string(node, name)
                            .as_deref()
                            .is_some_and(|s| s == "7" || s.eq_ignore_ascii_case("seven"))
                    })
            })
            .expect("native digit 7");
        assert_eq!(
            unsafe { native_digit.perform_action(&CFString::from_str("AXPress")) },
            AXError::Success
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let values: Vec<_> = native_ax_nodes(&native_window)
                .iter()
                .filter_map(|node| native_ax_string(node, "AXValue"))
                .collect();
            if values
                .iter()
                .any(|value| value.trim_matches(['\u{200e}', '\u{200f}']) == "7")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native same-digit display positive control: {values:?}"
            );
            thread::sleep(Duration::from_millis(30));
        }
        assert_eq!(
            unsafe { clear.perform_action(&CFString::from_str("AXPress")) },
            AXError::Success
        );
        let clear_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let values: Vec<_> = native_ax_nodes(&native_window)
                .iter()
                .filter_map(|node| native_ax_string(node, "AXValue"))
                .map(|value| value.trim_matches(['\u{200e}', '\u{200f}']).to_owned())
                .collect();
            if values.iter().any(|value| value == "0") && !values.iter().any(|value| value == "7") {
                break;
            }
            assert!(
                Instant::now() < clear_deadline,
                "clear must leave 7 and show zero before testing the driver press: {values:?}"
            );
            thread::sleep(Duration::from_millis(30));
        }
        let observed = live.observe("com.apple.calculator", None);
        let digit = observed
            .elements
            .iter()
            .find(|e| {
                e.role == "button"
                    && e.label
                        .as_deref()
                        .is_some_and(|s| s == "7" || s.eq_ignore_ascii_case("seven"))
            })
            .expect("digit 7");
        live.act_app(
            "com.apple.calculator",
            Some(observed.window.id),
            click_target(Target::Element { element: digit.id }),
            false,
            true,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let values: Vec<_> = native_ax_nodes(&native_window)
                .iter()
                .filter_map(|node| native_ax_string(node, "AXValue"))
                .collect();
            if values
                .iter()
                .any(|value| value.trim_matches(['\u{200e}', '\u{200f}']) == "7")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pressing observed digit must change native display to 7: {values:?}"
            );
            thread::sleep(Duration::from_millis(30));
        }
    });
}

#[test]
#[ignore]
fn finder_observe_survives_failed_subroles_and_scrolls_own_300_file_list_to_bottom() {
    run_live_test(|| {
        use objc2_application_services::AXUIElement;
        let Some(live) = Live::start() else {
            return;
        };
        let folder = live.directory.path().join("orca-cu-300-files");
        std::fs::create_dir(&folder).unwrap();
        for i in 1..=300 {
            std::fs::write(folder.join(format!("file-{i:03}.txt")), "").unwrap();
        }
        let script = format!(
            "tell application \"Finder\"\nset probe to make new Finder window to POSIX file \"{}\"\nset current view of probe to list view\nactivate\nreturn id of probe\nend tell",
            folder.display()
        );
        let output = Command::new("osascript")
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _owned_window = FinderWindow(
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        );
        let app = live
            .apps()
            .into_iter()
            .find(|a| a.bundle_id == "com.apple.finder")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let window = loop {
            if let Some(w) = live
                .apps()
                .into_iter()
                .find(|a| a.bundle_id == "com.apple.finder")
                .unwrap()
                .windows
                .into_iter()
                .find(|w| w.title == "orca-cu-300-files")
            {
                break w;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(50));
        };
        let application = unsafe { AXUIElement::new_application(app.pid.unwrap()) };
        let windows = fixture_ax_value(&application, "AXWindows").unwrap();
        let windows = windows
            .downcast_ref::<objc2_core_foundation::CFArray>()
            .unwrap();
        let native_window = (0..windows.len())
            .find_map(|i| {
                let value = unsafe {
                    &*windows
                        .value_at_index(i as isize)
                        .cast::<objc2_core_foundation::CFType>()
                };
                let w = value.downcast_ref::<AXUIElement>()?;
                (native_ax_string(w, "AXTitle").as_deref() == Some("orca-cu-300-files")).then(
                    || unsafe {
                        objc2_core_foundation::CFRetained::retain(std::ptr::NonNull::from(w))
                    },
                )
            })
            .expect("only our scratch Finder window");
        let nodes = native_ax_nodes(&native_window);
        let scroll = nodes
            .iter()
            .find(|n| native_ax_string(n, "AXRole").as_deref() == Some("AXScrollArea"))
            .expect("native list control");
        let bar = fixture_ax_value(scroll, "AXVerticalScrollBar")
            .unwrap()
            .downcast::<AXUIElement>()
            .unwrap();
        assert_eq!(
            native_ax_string(&bar, "AXValue").as_deref(),
            Some("0"),
            "native list starts at top"
        );
        for position in [1.0, 0.0] {
            assert_eq!(
                unsafe {
                    bar.set_attribute_value(
                        &objc2_core_foundation::CFString::from_str("AXValue"),
                        objc2_core_foundation::CFNumber::new_f64(position).as_ref(),
                    )
                },
                objc2_application_services::AXError::Success
            );
            assert_eq!(
                native_ax_string(&bar, "AXValue").as_deref(),
                Some(if position == 1.0 { "1" } else { "0" }),
                "native same-list scroll positive control"
            );
        }
        let observed = live.observe_window("com.apple.finder", Some(window.id), None, 1000, false);
        assert!(
            observed.elements.iter().any(|e| e.role == "scroll area"),
            "observed file list"
        );
        for _ in 0..10 {
            if native_ax_string(&bar, "AXValue").as_deref() == Some("1") {
                break;
            }
            let observed =
                live.observe_window("com.apple.finder", Some(window.id), None, 1000, false);
            let list = observed
                .elements
                .iter()
                .find(|e| e.role == "scroll area")
                .unwrap();
            live.act_app(
                "com.apple.finder",
                Some(window.id),
                Action::Scroll {
                    target: Target::Element { element: list.id },
                    direction: Direction::Down,
                    amount: 50,
                },
                false,
                true,
            )
            .unwrap();
        }
        assert_eq!(
            native_ax_string(&bar, "AXValue").as_deref(),
            Some("1"),
            "native list reaches bottom"
        );
    });
}

fn unreadable_subrole_requires_approval(code: i32) {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let option = format!("--subrole-error={code}");
        let (pid, _) = live.launch_fixture_with_flags(&["--prefilled-name", &option]);
        let window = fixture_window(pid);
        let name = native_ax_nodes(&window)
            .into_iter()
            .find(|e| native_ax_string(e, "AXDescription").as_deref() == Some("Name"))
            .unwrap();
        // Even Failure has a same-field native read/write control before observe.
        let initial = native_ax_string(&name, "AXValue").unwrap();
        assert_eq!(
            unsafe {
                name.set_attribute_value(
                    &objc2_core_foundation::CFString::from_str("AXValue"),
                    objc2_core_foundation::CFString::from_str(&initial).as_ref(),
                )
            },
            objc2_application_services::AXError::Success
        );
        assert_eq!(
            native_ax_string(&name, "AXValue").as_deref(),
            Some(initial.as_str())
        );
        let observed = live.observe("dev.orca.cu-fixture", None);
        let target = element_named(&observed, "Name");
        live.act(
            Action::Type {
                text: "u".into(),
                target: Some(target),
            },
            true,
        )
        .unwrap();
        let control = native_ax_string(&name, "AXValue").unwrap();
        assert!(
            control.ends_with('u'),
            "approved same-field positive control: {control}"
        );
        let result = live.act(
            Action::Type {
                text: "x".into(),
                target: Some(target),
            },
            false,
        );
        assert_eq!(
            native_ax_string(&name, "AXValue").as_deref(),
            Some(control.as_str()),
            "unapproved unreadable-subrole text must not be delivered"
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::NeedsSecureApproval);
        let observed = live.observe("dev.orca.cu-fixture", None);
        let field = observed
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Name"))
            .unwrap();
        assert!(field.secure, "unknown text subrole must be marked secure");
        assert!(
            field.value.is_none(),
            "unknown secure value must be redacted"
        );
    });
}

#[test]
#[ignore]
fn a_failed_text_subrole_is_secure_and_needs_approval() {
    unreadable_subrole_requires_approval(-25200);
}
#[test]
#[ignore]
fn a_missing_text_subrole_is_ordinary() {
    absent_subrole_is_ordinary(-25212);
}
#[test]
#[ignore]
fn an_unsupported_text_subrole_is_ordinary() {
    absent_subrole_is_ordinary(-25205);
}

fn one_bad_button_preserves_the_tree(flag: &str) {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live.launch_fixture_with_flags(&[flag, "--prefilled-name"]);
        let window = fixture_window(pid);
        let name = if flag == "--bad-button-children" {
            let application =
                unsafe { objc2_application_services::AXUIElement::new_application(pid) };
            fixture_ax_value(&application, "AXFocusedUIElement")
                .unwrap()
                .downcast::<objc2_application_services::AXUIElement>()
                .unwrap()
        } else {
            native_ax_nodes(&window)
                .into_iter()
                .find(|e| native_ax_string(e, "AXDescription").as_deref() == Some("Name"))
                .unwrap()
        };
        assert_eq!(
            unsafe {
                name.set_attribute_value(
                    &objc2_core_foundation::CFString::from_str("AXValue"),
                    objc2_core_foundation::CFString::from_str("native control").as_ref(),
                )
            },
            objc2_application_services::AXError::Success
        );
        assert_eq!(
            native_ax_string(&name, "AXValue").as_deref(),
            Some("native control"),
            "same-app healthy sibling positive control"
        );
        let observed = live.observe("dev.orca.cu-fixture", None);
        assert!(
            observed.screenshot.is_some(),
            "a failed local child read must not cost the window observation"
        );
        if flag != "--bad-button-children" {
            assert!(
                observed
                    .elements
                    .iter()
                    .any(|e| e.label.as_deref() == Some("Name"))
            );
            assert!(
                observed
                    .elements
                    .iter()
                    .any(|e| e.label.as_deref() == Some("Password"))
            );
        }
    });
}
#[test]
#[ignore]
fn one_failed_role_does_not_abort_observation() {
    one_bad_button_preserves_the_tree("--bad-button-role");
}
#[test]
#[ignore]
fn one_failed_frame_does_not_abort_observation() {
    one_bad_button_preserves_the_tree("--bad-button-frame");
}
#[test]
#[ignore]
fn one_failed_children_read_does_not_abort_observation() {
    one_bad_button_preserves_the_tree("--bad-button-children");
}

fn native_window_named(
    pid: i32,
    title: &str,
) -> objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement> {
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::{CFArray, CFRetained, CFType};
    let app = unsafe { AXUIElement::new_application(pid) };
    let windows = fixture_ax_value(&app, "AXWindows").unwrap();
    let windows = windows.downcast_ref::<CFArray>().unwrap();
    (0..windows.len())
        .find_map(|i| {
            let value = unsafe { &*windows.value_at_index(i as isize).cast::<CFType>() };
            let window = value.downcast_ref::<AXUIElement>()?;
            native_ax_string(window, "AXTitle")
                .filter(|t| t.contains(title))
                .map(|_| unsafe { CFRetained::retain(std::ptr::NonNull::from(window)) })
        })
        .expect("only the named test-owned window")
}

fn native_ax_pid(element: &objc2_application_services::AXUIElement) -> i32 {
    let mut pid = 0;
    assert_eq!(
        unsafe { element.pid(std::ptr::NonNull::from(&mut pid)) },
        objc2_application_services::AXError::Success
    );
    pid
}

#[test]
#[ignore]
fn textedit_save_panel_uses_foreground_for_typing() {
    run_live_test(|| {
        use objc2_application_services::AXUIElement;
        let Some(mut live) = Live::start() else {
            return;
        };
        if !bundle_pids("com.apple.TextEdit").is_empty() {
            eprintln!("skipped: TextEdit already running; preserve user documents");
            return;
        }
        let file = live.directory.path().join("orca-cu-save-panel.txt");
        std::fs::write(&file, "Seed.").unwrap();
        let app = launch_test_app(
            &mut live,
            "com.apple.TextEdit",
            vec![file.to_string_lossy().into_owned()],
        );
        let pid = app.pid.unwrap();
        let window = app
            .windows
            .iter()
            .find(|w| w.title.contains("orca-cu-save-panel.txt"))
            .unwrap()
            .id;
        let native_window = native_window_named(pid, "orca-cu-save-panel.txt");
        let editor = native_ax_nodes(&native_window)
            .into_iter()
            .find(|e| native_ax_string(e, "AXRole").as_deref() == Some("AXTextArea"))
            .unwrap();
        live.make_finder_frontmost();
        let observed = live.observe_window("com.apple.TextEdit", Some(window), None, 300, false);
        let area = observed
            .elements
            .iter()
            .find(|e| e.role == "text area")
            .unwrap();
        let control = live
            .act_app(
                "com.apple.TextEdit",
                Some(window),
                Action::Type {
                    text: "Background control".into(),
                    target: Some(Target::Element { element: area.id }),
                },
                false,
                false,
            )
            .unwrap();
        assert!(!control.foreground_used);
        assert_eq!(
            native_ax_string(&editor, "AXValue").as_deref(),
            Some("Seed.Background control"),
            "main-editor background positive control"
        );
        live.assert_frontmost_and_pointer_unchanged();
        live.act_app(
            "com.apple.TextEdit",
            Some(window),
            Action::Key {
                keys: vec!["cmd".into(), "shift".into(), "option".into(), "s".into()],
                target: None,
            },
            false,
            true,
        )
        .unwrap();
        let application = unsafe { AXUIElement::new_application(pid) };
        let deadline = Instant::now() + Duration::from_secs(5);
        let name = loop {
            if let Some(focus) = native_ax_optional_value(&application, "AXFocusedUIElement")
                .and_then(|v| v.downcast::<AXUIElement>().ok())
                && native_ax_string(&focus, "AXValue")
                    .is_some_and(|v| v.contains("orca-cu-save-panel"))
            {
                break focus;
            }
            assert!(Instant::now() < deadline, "Save As name field not focused");
            thread::sleep(Duration::from_millis(30));
        };
        let native_fields: Vec<_> = native_ax_nodes(&native_window)
            .into_iter()
            .filter(|e| {
                native_ax_string(e, "AXRole")
                    .as_deref()
                    .is_some_and(|r| r == "AXTextField" || r == "AXComboBox")
            })
            .map(|e| {
                (
                    native_ax_string(&e, "AXDescription"),
                    native_ax_string(&e, "AXValue"),
                    native_ax_pid(&e),
                )
            })
            .collect();
        eprintln!(
            "test-owned Save panel native fields: {native_fields:?}; focus owner={} app={pid}",
            native_ax_pid(&name)
        );
        for attribute in ["AXTopLevelUIElement", "AXWindow"] {
            let shortcut = native_ax_optional_value(&name, attribute)
                .and_then(|value| value.downcast::<AXUIElement>().ok());
            let metadata = shortcut.as_ref().map(|element| {
                (
                    native_ax_string(element, "AXRole"),
                    native_ax_string(element, "AXSubrole"),
                    native_ax_optional_value(element, "AXModal").and_then(|value| {
                        value
                            .downcast_ref::<objc2_core_foundation::CFBoolean>()
                            .map(|value| value.value())
                    }),
                    native_ax_pid(element),
                )
            });
            eprintln!("test-owned Save field {attribute}: {metadata:?}");
        }
        let mut ancestor = Some(name.clone());
        for depth in 0..12 {
            let Some(node) = ancestor else { break };
            let modal = native_ax_optional_value(&node, "AXModal").and_then(|v| {
                v.downcast_ref::<objc2_core_foundation::CFBoolean>()
                    .map(|b| b.value())
            });
            eprintln!(
                "own panel ancestor {depth}: role={:?}, subrole={:?}, modal={modal:?}, owner={}",
                native_ax_string(&node, "AXRole"),
                native_ax_string(&node, "AXSubrole"),
                native_ax_pid(&node)
            );
            ancestor = native_ax_optional_value(&node, "AXParent")
                .and_then(|v| v.downcast::<AXUIElement>().ok());
        }
        let before = native_ax_string(&name, "AXValue").unwrap();
        let observed = live.observe_window("com.apple.TextEdit", Some(window), None, 300, false);
        eprintln!(
            "Save panel name label: {:?}, native role={:?}, owner={}, app={pid}, observed fields={:?}",
            native_ax_string(&name, "AXDescription"),
            native_ax_string(&name, "AXRole"),
            native_ax_pid(&name),
            observed
                .elements
                .iter()
                .filter(|e| e.role == "text field" || e.role == "combo box")
                .map(|e| (&e.label, e.frame))
                .collect::<Vec<_>>()
        );
        let outcome = live
            .act_app(
                "com.apple.TextEdit",
                Some(window),
                Action::Type {
                    text: "orca-panel-name".into(),
                    target: None,
                },
                false,
                true,
            )
            .unwrap();
        let after = native_ax_string(&name, "AXValue").unwrap();
        assert_ne!(after, before, "reported done but Save name did not change");
        assert!(
            after.contains("orca-panel-name"),
            "native filename receipt: {after:?}"
        );
        assert!(
            outcome.foreground_used,
            "Save panel recipient must use HID fallback"
        );
        eprintln!("panel input source: {:?}", current_keyboard_input_source());
        let current = live.observe_window("com.apple.TextEdit", Some(window), None, 300, false);
        let field = current
            .elements
            .iter()
            .find(|e| e.role == "text field" && e.label.is_none())
            .expect("observed test-owned filename field");
        let outcome = live
            .act_app(
                "com.apple.TextEdit",
                Some(window),
                Action::Type {
                    text: "-target".into(),
                    target: Some(Target::Element { element: field.id }),
                },
                false,
                true,
            )
            .unwrap();
        assert!(outcome.foreground_used);
        let targeted_value = native_ax_string(&name, "AXValue").unwrap();
        eprintln!("element-targeted panel receipt: {targeted_value:?}");
        assert!(
            targeted_value.ends_with("-target"),
            "element-targeted panel receipt: {targeted_value:?}"
        );
        let before_key = native_ax_string(&name, "AXValue").unwrap();
        let outcome = live
            .act_app(
                "com.apple.TextEdit",
                Some(window),
                Action::Key {
                    keys: vec!["space".into()],
                    target: None,
                },
                false,
                true,
            )
            .unwrap();
        assert!(outcome.foreground_used);
        assert_ne!(
            native_ax_string(&name, "AXValue").unwrap(),
            before_key,
            "text-key panel receipt"
        );
        // Cancel only our Save As panel; never save or browse outside the temporary document.
        live.act_app(
            "com.apple.TextEdit",
            Some(window),
            Action::Key {
                keys: vec!["escape".into()],
                target: None,
            },
            false,
            true,
        )
        .unwrap();
        live.assert_frontmost_and_pointer_unchanged();
    });
}

fn background_300_roundtrip(
    live: &Live,
    app: &str,
    window: u32,
    editor: &objc2_application_services::AXUIElement,
    log: Option<&std::path::Path>,
) {
    let observed = live.observe_window(app, Some(window), None, 300, false);
    let element = observed
        .elements
        .iter()
        .find(|e| e.role == "text area")
        .unwrap()
        .id;
    let initial = native_ax_string(editor, "AXValue").unwrap();
    let control = live
        .act_app(
            app,
            Some(window),
            Action::Type {
                text: "AbC/9".into(),
                target: Some(Target::Element { element }),
            },
            false,
            false,
        )
        .unwrap();
    assert!(!control.foreground_used);
    assert_eq!(
        native_ax_string(editor, "AXValue").unwrap(),
        format!("{initial}AbC/9"),
        "same-editor short background positive control"
    );
    assert_eq!(
        unsafe {
            editor.set_attribute_value(
                &objc2_core_foundation::CFString::from_str("AXValue"),
                objc2_core_foundation::CFString::from_str("").as_ref(),
            )
        },
        objc2_application_services::AXError::Success
    );
    assert_eq!(native_ax_string(editor, "AXValue").as_deref(), Some(""));
    let text: String = "\nAbC/tmp/OrCa🦀/xyZ\n"
        .repeat(20)
        .chars()
        .take(300)
        .collect();
    assert_eq!(text.chars().count(), 300);
    let observed = live.observe_window(app, Some(window), None, 300, false);
    let element = observed
        .elements
        .iter()
        .find(|e| e.role == "text area")
        .unwrap()
        .id;
    let result = live
        .act_app(
            app,
            Some(window),
            Action::Type {
                text: text.clone(),
                target: Some(Target::Element { element }),
            },
            false,
            false,
        )
        .unwrap();
    assert!(!result.foreground_used, "long input must remain background");
    thread::sleep(Duration::from_millis(200));
    let actual = native_ax_string(editor, "AXValue").unwrap();
    if let Some(log) = log {
        let events: Vec<serde_json::Value> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|e: &serde_json::Value| e["type"] == "keydown")
            .collect();
        eprintln!("test-owned keydown receipts: {events:?}");
    }
    eprintln!(
        "300-character native receipt: expected_chars={}, actual_chars={}, actual={actual:?}",
        text.chars().count(),
        actual.chars().count()
    );
    assert_eq!(
        actual, text,
        "background type must preserve every character, newline, slash, case and emoji"
    );
    live.assert_frontmost_and_pointer_unchanged();
}

#[test]
#[ignore]
fn background_type_300_characters_roundtrips_in_fixture_text_area() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, log) = live.launch_fixture();
        let window = fixture_window(pid);
        let editor = fixture_ax_role(&window, "AXTextArea").unwrap();
        live.make_finder_frontmost();
        let observed = live.observe("dev.orca.cu-fixture", None);
        background_300_roundtrip(
            &live,
            "dev.orca.cu-fixture",
            observed.window.id,
            &editor,
            Some(&log),
        );
    });
}

#[test]
#[ignore]
fn background_type_300_characters_roundtrips_in_textedit_document() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        if !bundle_pids("com.apple.TextEdit").is_empty() {
            eprintln!("skipped: TextEdit already running");
            return;
        }
        let file = live.directory.path().join("orca-cu-300.txt");
        std::fs::write(&file, "").unwrap();
        let app = launch_test_app(
            &mut live,
            "com.apple.TextEdit",
            vec![file.to_string_lossy().into_owned()],
        );
        let window = app
            .windows
            .iter()
            .find(|w| w.title.contains("orca-cu-300.txt"))
            .unwrap()
            .id;
        let native = native_window_named(app.pid.unwrap(), "orca-cu-300.txt");
        let editor = fixture_ax_role(&native, "AXTextArea").unwrap();
        live.make_finder_frontmost();
        background_300_roundtrip(&live, "com.apple.TextEdit", window, &editor, None);
    });
}

#[test]
#[ignore]
fn a_running_localized_textedit_resolves_by_its_english_name_for_observe() {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        if !bundle_pids("com.apple.TextEdit").is_empty() {
            eprintln!("skipped: TextEdit already running");
            return;
        }
        let file = live.directory.path().join("orca-cu-english-name.txt");
        std::fs::write(&file, "Own temporary English-name observe control").unwrap();
        let app = launch_test_app(
            &mut live,
            "com.apple.TextEdit",
            vec![file.to_string_lossy().into_owned()],
        );
        let window = app
            .windows
            .iter()
            .find(|w| w.title.contains("orca-cu-english-name.txt"))
            .unwrap()
            .id;
        // The bundle-ID observe of that same owned window is the positive control.
        let control = live.observe_window("com.apple.TextEdit", Some(window), None, 300, false);
        assert!(control.elements.iter().any(|e| e.role == "text area"));
        eprintln!(
            "running TextEdit localized name: {:?}, pid={:?}",
            app.name, app.pid
        );
        let reply = live
            .client
            .call(
                Request::ResolveApp {
                    app: "TextEdit".into(),
                    allow_not_running: false,
                },
                Duration::from_secs(30),
            )
            .unwrap();
        let Reply::Resolved { app: resolved } = reply else {
            panic!("resolved app")
        };
        assert_eq!(resolved.bundle_id, "com.apple.TextEdit");
        assert_eq!(resolved.pid, app.pid);
        let observed = live.observe_window(&resolved.bundle_id, Some(window), None, 300, false);
        assert!(observed.elements.iter().any(|e| e.role == "text area"));
    });
}

fn cold_launch_by_name(name: &str, bundle: &str) {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let before = bundle_pids(bundle);
        if !before.is_empty() {
            eprintln!("skipped: {name} already running");
            return;
        }
        let open = if bundle == "com.apple.TextEdit" {
            let file = live.directory.path().join("orca-cu-cold-name.txt");
            std::fs::write(&file, "Own cold-launch document").unwrap();
            vec![file.to_string_lossy().into_owned()]
        } else {
            vec![]
        };
        let Reply::Resolved { app: approved } = live
            .client
            .call(
                Request::ResolveApp {
                    app: name.into(),
                    allow_not_running: true,
                },
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("resolved cold app name");
        };
        assert_eq!(approved.bundle_id, bundle);
        eprintln!(
            "cold name {name:?} resolved to approved ID {:?}",
            approved.bundle_id
        );
        let result = live.client.call(
            Request::LaunchApp {
                app: approved.bundle_id,
                open,
            },
            Duration::from_secs(60),
        );
        register_launch_pids(&mut live.launched, &before, &bundle_pids(bundle));
        let Reply::Launched { app } = result.unwrap() else {
            panic!("launched app")
        };
        assert_eq!(app.bundle_id, bundle);
        assert!(app.pid.is_some() && !app.windows.is_empty());
        if bundle == "com.apple.TextEdit" {
            let window = app
                .windows
                .iter()
                .find(|w| w.title.contains("orca-cu-cold-name.txt"))
                .unwrap()
                .id;
            let observed = live.observe_window(bundle, Some(window), None, 300, false);
            assert!(observed.elements.iter().any(|e| e.role == "text area"));
        }
    });
}
#[test]
#[ignore]
fn cold_textedit_launches_by_english_name() {
    cold_launch_by_name("TextEdit", "com.apple.TextEdit");
}
#[test]
#[ignore]
fn cold_calculator_launches_by_english_name() {
    cold_launch_by_name("Calculator", "com.apple.calculator");
}

fn absent_subrole_is_ordinary(code: i32) {
    run_live_test(|| {
        let Some(mut live) = Live::start() else {
            return;
        };
        let option = format!("--subrole-error={code}");
        live.launch_fixture_with_flags(&["--prefilled-name", &option]);
        let observation = live.observe("dev.orca.cu-fixture", None);
        let field = observation
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Name"))
            .unwrap();
        assert!(!field.secure);
        assert_eq!(field.value.as_deref(), Some("a🦀c"));
        live.act(
            Action::Type {
                text: "x".into(),
                target: Some(element_named(&observation, "Name")),
            },
            false,
        )
        .unwrap();
        assert_eq!(live.fixture_control_value("Name"), "a🦀cx");
    });
}

#[test]
#[ignore]
fn literal_type_refuses_single_line_controls_and_inserts_tabs_in_text_areas() {
    run_live_test(|| {
        use objc2_core_foundation::CFString;
        let Some(mut live) = Live::start() else {
            return;
        };
        let (pid, _) = live.launch_fixture();
        live.make_finder_frontmost();
        let observation = live.observe("dev.orca.cu-fixture", None);
        let name = element_named(&observation, "Name");
        for text in ["user\tpass", "user\npass", "user\rpass"] {
            let failure = live
                .act(
                    Action::Type {
                        text: text.into(),
                        target: Some(name),
                    },
                    false,
                )
                .unwrap_err();
            assert_eq!(failure.kind, ErrorKind::Failed);
            assert!(failure.message.contains("[\"tab\"]"));
            assert!(failure.message.contains("[\"return\"]"));
            assert_eq!(live.fixture_control_value("Name"), "");
            assert_eq!(
                password_length(pid),
                0,
                "no unapproved text may reach Password"
            );
        }
        // The same single-line recipient accepts ordinary text without approval.
        live.act(
            Action::Type {
                text: "user".into(),
                target: Some(name),
            },
            false,
        )
        .unwrap();
        assert_eq!(live.fixture_control_value("Name"), "user");
        assert_eq!(password_length(pid), 0);
        let window = fixture_window(pid);
        let editor = fixture_ax_role(&window, "AXTextArea").unwrap();
        assert_eq!(
            unsafe {
                editor.set_attribute_value(
                    &CFString::from_str("AXValue"),
                    CFString::from_str("").as_ref(),
                )
            },
            objc2_application_services::AXError::Success
        );
        let observation = live.observe("dev.orca.cu-fixture", None);
        let area = observation
            .elements
            .iter()
            .find(|e| e.role == "text area")
            .unwrap()
            .id;
        let outcome = live
            .act(
                Action::Type {
                    text: "user\tpass".into(),
                    target: Some(Target::Element { element: area }),
                },
                false,
            )
            .unwrap();
        assert!(!outcome.foreground_used);
        assert_eq!(
            native_ax_string(&editor, "AXValue").as_deref(),
            Some("user\tpass")
        );
        assert_eq!(password_length(pid), 0);
        live.assert_frontmost_and_pointer_unchanged();
    });
}
