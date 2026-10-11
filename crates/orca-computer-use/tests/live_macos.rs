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
    record_restoration(frontmost, pointer);
    let failures = RESTORATION_FAILURES.lock().unwrap();
    for failure in failures.iter() {
        eprintln!("GUI restoration failed: {failure}");
    }
    if passed && failures.is_empty() {
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
use std::thread;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_core_graphics::{CGEvent, CGWarpMouseCursorPosition};
use objc2_foundation::NSString;
use orca_computer_use::client::{DriverClient, HelloInfo};
use orca_computer_use::driver::macos::MacDriver;
use orca_computer_use::inprocess::InProcessLauncher;
use orca_computer_use::protocol::{AppInfo, ErrorKind, Observation, ObserveParams, Reply, Request};

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
</dict></plist>"#).unwrap();
    let output = Command::new("/usr/bin/swiftc")
        .arg("-O")
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
        let running = workspace
            .runningApplications()
            .iter()
            .map(|app| app.processIdentifier())
            .collect();
        let pointer = CGEvent::location(Some(&CGEvent::new(None).expect("read pointer position")));
        let directory = tempfile::tempdir().unwrap();
        let launcher =
            InProcessLauncher::new(MacDriver::new(directory.path().join("computer-use.lock")));
        let (client, hello) = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap();
        let live = Self {
            client,
            hello,
            directory,
            frontmost,
            pointer,
            running,
            launched: HashSet::new(),
        };
        let permissions = live.hello.permissions;
        if !permissions.accessibility || !permissions.screen_recording || !permissions.event_tap {
            eprintln!("skipped: missing permission: {permissions:?}");
            return None;
        }
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

    fn launch_fixture(&mut self) -> (i32, PathBuf) {
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
                }) {
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
    use objc2_application_services::{AXError, AXUIElement};
    use objc2_core_foundation::{CFBoolean, CFString};
    use objc2_core_graphics::CGError;
    let mut failures = vec![];
    if let Some(pid) = frontmost {
        let app = unsafe { AXUIElement::new_application(pid) };
        let timeout = unsafe { app.set_messaging_timeout(2.0) };
        if timeout != AXError::Success {
            failures.push(format!("AX timeout for PID {pid}: {timeout:?}"));
        }
        let restored = unsafe {
            app.set_attribute_value(
                &CFString::from_str("AXFrontmost"),
                CFBoolean::new(true).as_ref(),
            )
        };
        if restored != AXError::Success {
            failures.push(format!("frontmost restore for PID {pid}: {restored:?}"));
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
                failures.push(format!("frontmost PID {pid} was not restored"));
            }
        }
    }
    let warped = CGWarpMouseCursorPosition(pointer);
    if warped != CGError::Success {
        failures.push(format!("pointer warp failed: {warped:?}"));
    }
    let actual = CGEvent::new(None).map(|event| CGEvent::location(Some(&event)));
    if actual.is_none_or(|actual| {
        (actual.x - pointer.x).abs() > 1.0 || (actual.y - pointer.y).abs() > 1.0
    }) {
        failures.push(format!(
            "pointer not restored: expected {pointer:?}, actual {actual:?}"
        ));
    }
    RESTORATION_FAILURES.lock().unwrap().extend(failures);
}

#[test]
#[ignore]
fn hello_reports_permissions_the_os_and_the_host_terminal() {
    let Some(live) = Live::start() else { return };
    assert!(live.hello.macos_major >= 14);
    if let Ok(bundle_id) = std::env::var("__CFBundleIdentifier") {
        assert_eq!(
            live.hello.host_bundle_id.as_deref(),
            Some(bundle_id.as_str())
        );
    }
}

#[test]
#[ignore]
fn list_and_resolve_find_the_fixture_by_name_and_bundle_id() {
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
}

#[test]
#[ignore]
fn a_second_driver_finds_the_session_lock_held() {
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
}

#[test]
#[ignore]
fn launch_app_opens_a_file_in_textedit() {
    let Some(mut live) = Live::start() else {
        return;
    };
    let file = live.directory.path().join("orca-cu-test.txt");
    std::fs::write(&file, "Orca computer use fixture document\n").unwrap();
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
    assert_eq!(app.bundle_id, "com.apple.TextEdit");
    assert!(app.pid.is_some());
    assert!(
        !app.windows.is_empty(),
        "TextEdit opened the temporary file"
    );
}

#[test]
#[ignore]
fn opening_a_nonexistent_file_in_already_running_textedit_returns_an_error() {
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
}

#[test]
#[ignore]
fn cleanup_only_selects_explicitly_launched_processes() {
    let running = HashSet::from([10]);
    // PID 77 represents an unrelated app that appeared during the test.
    let launched = HashSet::from([10, 42]);
    let pids = cleanup_pids(&running, &launched);
    assert_eq!(pids, vec![42]);
    assert!(!pids.contains(&77));
}

#[test]
#[ignore]
fn cleanup_preserves_an_app_launched_by_another_live_session() {
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
}

#[test]
#[ignore]
fn launch_attribution_registers_only_pids_that_appeared_during_the_request() {
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
}

#[test]
#[ignore]
fn observe_lists_the_fixture_controls_in_screenshot_pixels() {
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
}

#[test]
#[ignore]
fn observe_with_a_query_keeps_matches_and_their_ancestors() {
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
    assert!(
        descendants.elements.iter().any(
            |e| e.role == "text area" && e.value.as_ref().is_some_and(|v| v.contains("line 1"))
        )
    );
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
        assert!(
            Instant::now() < deadline,
            "Finder must actually cover the fixture"
        );
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
}

#[test]
#[ignore]
fn observe_of_an_electron_app_waits_for_its_tree() {
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
    let before = bundle_pids(&bundle);
    let result = live.client.call(
        Request::LaunchApp {
            app: bundle.clone(),
            open: vec![],
        },
        Duration::from_secs(60),
    );
    register_launch_pids(&mut live.launched, &before, &bundle_pids(&bundle));
    result.unwrap();
    let observation = live.observe(&bundle, None);
    assert!(!observation.no_element_tree);
    assert!(
        observation
            .elements
            .iter()
            .any(|e| e.role == "button" || e.role == "text field")
    );
}
