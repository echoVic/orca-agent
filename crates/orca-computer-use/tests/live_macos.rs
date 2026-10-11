#![cfg(target_os = "macos")]

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_core_graphics::{CGEvent, CGWarpMouseCursorPosition};
use objc2_foundation::NSString;
use orca_computer_use::client::{DriverClient, HelloInfo};
use orca_computer_use::driver::macos::MacDriver;
use orca_computer_use::inprocess::InProcessLauncher;
use orca_computer_use::protocol::{AppInfo, ErrorKind, Reply, Request};

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

    fn launch_fixture(&mut self) -> (i32, PathBuf) {
        let log = self.directory.path().join("fixture.jsonl");
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
            if let Some(app) = self.apps().into_iter().find(|app| {
                app.bundle_id == "dev.orca.cu-fixture"
                    && app.pid.is_some_and(|pid| !self.running.contains(&pid))
            }) {
                let pid = app.pid.unwrap();
                self.launched.insert(pid);
                if app
                    .windows
                    .iter()
                    .any(|window| window.title == "Orca CU Fixture")
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
        for bundle_id in ["dev.orca.cu-fixture", "com.apple.TextEdit"] {
            for app in NSRunningApplication::runningApplicationsWithBundleIdentifier(
                &NSString::from_str(bundle_id),
            ) {
                if !self.running.contains(&app.processIdentifier()) {
                    self.launched.insert(app.processIdentifier());
                }
            }
        }
        for pid in &self.launched {
            if !self.running.contains(pid)
                && let Some(app) =
                    NSRunningApplication::runningApplicationWithProcessIdentifier(*pid)
            {
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
        if let Some(pid) = self.frontmost {
            restore_frontmost(pid);
        }
        CGWarpMouseCursorPosition(self.pointer);
    }
}

fn restore_frontmost(pid: i32) {
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::{CFBoolean, CFString};
    let app = unsafe { AXUIElement::new_application(pid) };
    unsafe {
        app.set_messaging_timeout(2.0);
        app.set_attribute_value(
            &CFString::from_str("AXFrontmost"),
            CFBoolean::new(true).as_ref(),
        );
    }
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
    let Some(live) = Live::start() else { return };
    assert!(
        live.hello.macos_major >= 14,
        "first driver holds the same lock successfully"
    );
    let launcher = InProcessLauncher::new(MacDriver::new(
        live.directory.path().join("computer-use.lock"),
    ));
    let error = DriverClient::start(&launcher, Arc::new(|_| {})).unwrap_err();
    assert_eq!(error.kind, ErrorKind::LockHeld);
    assert_eq!(error.message, "another Orca session is using the computer");
}

#[test]
#[ignore]
fn launch_app_opens_a_file_in_textedit() {
    let Some(mut live) = Live::start() else {
        return;
    };
    let file = live.directory.path().join("orca-cu-test.txt");
    std::fs::write(&file, "Orca computer use fixture document\n").unwrap();
    // Track newly started TextEdit even if the request fails, so Drop still cleans up.
    let result = live.client.call(
        Request::LaunchApp {
            app: "com.apple.TextEdit".into(),
            open: vec![file.to_string_lossy().into_owned()],
        },
        Duration::from_secs(60),
    );
    for app in NSWorkspace::new().runningApplications() {
        if app
            .bundleIdentifier()
            .is_some_and(|id| id.to_string() == "com.apple.TextEdit")
            && !live.running.contains(&app.processIdentifier())
        {
            live.launched.insert(app.processIdentifier());
        }
    }
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
