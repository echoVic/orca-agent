mod actions;
#[cfg(feature = "test-utils")]
pub use actions::{
    test_last_action_deadline, test_native_keyboard_recipient, test_native_modality,
    test_native_type_role,
};
mod apps;
mod ax;
mod capture;
mod esc_tap;
mod events;
#[cfg(feature = "test-utils")]
pub use apps::{test_resolve_app_from_roots, test_resolve_with_running_from_roots};
#[cfg(feature = "test-utils")]
pub use events::{
    last_test_key_down, last_test_process_hid_post, test_check_click_point, test_click_at_point,
    test_foreground_text, test_mouse_down_count, with_held_test_input,
};
#[cfg(feature = "test-utils")]
pub use foreground::test_element_click_point;
mod foreground;
mod occlusion;
mod permissions;
mod tree;
mod windows;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    geometry,
    protocol::{Observation, ObserveParams, WindowInfo},
};
use ax::AxElement;
use tree::ElementTable;

use objc2::rc::autoreleasepool;
use objc2_foundation::NSProcessInfo;
use orca_platform::fs::ExclusiveFileLock;

use crate::protocol::{DriverError, ErrorKind, PROTOCOL_VERSION, Reply, Request};
use crate::serve::{DriverHandler, LineWriter};

pub struct MacDriver {
    lock_path: PathBuf,
    lock: Option<ExclusiveFileLock>,
    esc_tap: Option<esc_tap::EscTap>,
    aborted: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    stdin_fd: Option<std::os::fd::RawFd>,
    hello_complete: bool,
    pub(crate) element_tables: HashMap<(i32, u32), ElementTable>,
    accessibility_enabled: HashSet<i32>,
}

pub(crate) struct StopSignal {
    stopping: Arc<AtomicBool>,
    aborted: Arc<AtomicBool>,
}
impl StopSignal {
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.aborted.store(true, Ordering::SeqCst);
    }
}
impl MacDriver {
    pub fn new(lock_path: PathBuf) -> Self {
        Self {
            lock_path,
            lock: None,
            esc_tap: None,
            aborted: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            stdin_fd: None,
            hello_complete: false,
            element_tables: HashMap::new(),
            accessibility_enabled: HashSet::new(),
        }
    }

    pub(crate) fn watch_stdin(&mut self, fd: std::os::fd::RawFd) {
        self.stdin_fd = Some(fd);
    }
    pub(crate) fn stop_signal(&self) -> StopSignal {
        StopSignal {
            stopping: self.stopping.clone(),
            aborted: self.aborted.clone(),
        }
    }
    fn observe(&mut self, params: ObserveParams) -> Result<Observation, DriverError> {
        capture::check_unlocked()?;
        let app = apps::resolve_approved(&params.app)?;
        let pid = app
            .pid
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        let application = AxElement::application(pid)?;
        if self.accessibility_enabled.insert(pid) {
            let _ = application.set_bool("AXManualAccessibility", true);
        }
        let windows = capture::windows()?;
        let (ax_window, window) =
            resolve_window(&application, pid, &app.name, &windows, params.window)?;
        let window_frame = ax_window.position_size()?;
        if window_frame.iter().any(|v| !v.is_finite())
            || window_frame[2] <= 0.0
            || window_frame[3] <= 0.0
        {
            return Err(error(ErrorKind::Failed, "The window has invalid geometry"));
        }
        let screenshot = params
            .include_screenshot
            .then(|| capture::screenshot(window))
            .transpose()?;
        let image_size = screenshot
            .as_ref()
            .map(|s| (s.width, s.height))
            .unwrap_or_else(|| {
                geometry::screenshot_size(
                    (window.frame[2] * window.scale).round().max(1.0) as u32,
                    (window.frame[3] * window.scale).round().max(1.0) as u32,
                )
            });
        let geometry = geometry::CaptureGeometry {
            window_origin: (window_frame[0], window_frame[1]),
            window_size: (window_frame[2], window_frame[3]),
            image_size,
        };
        let tree::CollectedTree {
            elements,
            entries,
            omitted,
            no_element_tree,
            truncated,
        } = tree::collect(
            &ax_window,
            geometry,
            params.query.as_deref(),
            params.max_elements,
        )?;
        let focused_secure = match application.optional_attribute_element("AXFocusedUIElement")? {
            Some(element) => match element.is_secure() {
                Err(failure) if failure.kind == ErrorKind::Stale => true,
                result => result?,
            },
            None => false,
        };
        self.element_tables.insert(
            (pid, window.id),
            ElementTable {
                pid,
                window_id: window.id,
                window_frame,
                geometry,
                entries,
            },
        );
        Ok(Observation {
            bundle_id: app.bundle_id,
            app_name: app.name,
            window: WindowInfo {
                id: window.id,
                title: window.title.clone(),
                minimized: false,
            },
            elements,
            omitted,
            no_element_tree,
            truncated,
            focused_secure,
            screenshot,
        })
    }
}

pub(super) fn error(kind: ErrorKind, message: impl Into<String>) -> DriverError {
    DriverError {
        kind,
        message: message.into(),
        candidates: vec![],
    }
}

impl DriverHandler for MacDriver {
    fn handle(&mut self, request: Request, events: &LineWriter) -> Result<Reply, DriverError> {
        if let Some(fd) = self.stdin_fd {
            let input = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            if !matches!(crate::driver_input::peer_closed(input), Ok(false)) {
                self.stop_signal().stop();
            }
        }
        if self.stopping.load(Ordering::SeqCst) && !matches!(request, Request::Shutdown) {
            return Err(error(
                ErrorKind::Failed,
                "The driver is stopping because stdin closed",
            ));
        }
        if !matches!(request, Request::Hello { .. } | Request::Shutdown)
            && (!self.hello_complete || self.lock.is_none())
        {
            if self.lock.is_none() {
                // Probe contention without granting a handshake or retaining a free lock.
                drop(crate::session_lock::acquire(&self.lock_path)?);
            }
            return Err(error(ErrorKind::Failed, "send hello first"));
        }
        autoreleasepool(|_| match request {
            Request::Hello { protocol } => {
                if self.lock.is_none() {
                    self.lock = Some(crate::session_lock::acquire(&self.lock_path)?);
                }
                if protocol != PROTOCOL_VERSION {
                    self.shutdown();
                    return Err(error(
                        ErrorKind::VersionMismatch,
                        "Orca was updated; start a new session to use computer use",
                    ));
                }
                self.hello_complete = true;
                Ok(Reply::Hello {
                    protocol: PROTOCOL_VERSION,
                    macos_major: NSProcessInfo::processInfo()
                        .operatingSystemVersion()
                        .majorVersion as u32,
                    host_bundle_id: permissions::host_bundle_id(),
                    permissions: permissions::check(),
                })
            }
            Request::ListApps => Ok(Reply::Apps {
                apps: apps::list_verified()?,
            }),
            Request::ResolveApp {
                app,
                allow_not_running,
            } => Ok(Reply::Resolved {
                app: apps::resolve(&app, allow_not_running)?,
            }),
            Request::LaunchApp { app, open } => Ok(Reply::Launched {
                app: apps::launch(&app, &open)?,
            }),
            Request::Turn { active } => {
                if active && self.esc_tap.is_none() {
                    self.aborted.store(false, Ordering::SeqCst);
                    self.esc_tap = Some(esc_tap::EscTap::start(
                        events.clone(),
                        self.aborted.clone(),
                    )?);
                } else if !active {
                    self.esc_tap.take();
                    self.aborted.store(false, Ordering::SeqCst);
                }
                Ok(Reply::Done)
            }
            Request::Shutdown => {
                self.shutdown();
                Ok(Reply::Done)
            }
            Request::Observe(params) => Ok(Reply::Observed {
                observation: self.observe(params)?,
            }),
            Request::Act(params) => Ok(Reply::Acted {
                outcome: self.act(params)?,
            }),
        })
    }

    fn shutdown(&mut self) {
        self.hello_complete = false;
        self.esc_tap.take();
        self.aborted.store(false, Ordering::SeqCst);
        self.lock.take();
        self.element_tables.clear();
        self.accessibility_enabled.clear();
    }
}

pub(super) fn frames_match(ax: [f64; 4], capture: [f64; 4]) -> bool {
    ax.into_iter()
        .zip(capture)
        .all(|(a, b)| (a - b).abs() <= 2.0)
}

/// Whether the app shows any window; unreadable AX state fails closed as an error.
pub(super) fn app_has_window(application: &AxElement) -> Result<bool, DriverError> {
    Ok(windows::has_window(
        application.elements("AXWindows")?.len(),
        application.attribute_element("AXMainWindow")?.is_some(),
    ))
}

/// Act and observe resolve the same live window, including apps with no main window.
fn resolve_window<'a>(
    application: &AxElement,
    pid: i32,
    app_name: &str,
    windows: &'a [capture::Window],
    id: Option<u32>,
) -> Result<(AxElement, &'a capture::Window), DriverError> {
    let mut candidates = application.elements("AXWindows")?;
    let main = application.attribute_element("AXMainWindow")?;
    if candidates.is_empty()
        && let Some(main) = &main
    {
        candidates.push(main.clone());
    }
    // One unreadable helper window must not block the app, and a hung app
    // must not hold observe/act past this budget.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let (mut candidates, facts) = windows::gather_facts(candidates, deadline, |candidate, left| {
        let _ = unsafe { candidate.0.set_messaging_timeout(left.min(2.0)) };
        (
            candidate.position_size().ok(),
            candidate.attribute_string("AXSubrole").ok().flatten(),
            candidate.attribute_bool("AXMinimized").ok().flatten(),
        )
    });
    let main_index = main
        .as_ref()
        .and_then(|main| candidates.iter().position(|candidate| candidate == main));
    let (ax_index, capture_index) =
        windows::choose_window(id, pid, app_name, windows, &facts, main_index)?;
    let chosen = candidates.swap_remove(ax_index);
    // The budget may have shortened this handle's timeout; observe/act need the usual one.
    let _ = unsafe { chosen.0.set_messaging_timeout(2.0) };
    Ok((chosen, &windows[capture_index]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_require_a_successful_hello_even_when_the_lock_is_free() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("computer-use.lock");
        let mut driver = MacDriver::new(path.clone());
        let output = LineWriter::new(std::io::sink());
        for request in [
            Request::Observe(ObserveParams {
                app: "dev.orca.not-installed".into(),
                window: None,
                query: None,
                max_elements: 300,
                include_screenshot: true,
            }),
            Request::ListApps,
            Request::Act(crate::protocol::ActParams {
                app: "dev.orca.not-installed".into(),
                window: None,
                action: crate::protocol::Action::Type {
                    text: "abc".into(),
                    target: None,
                },
                foreground_fallback: false,
                observe_after: false,
                include_screenshot: false,
                secure_approved: false,
            }),
            Request::Turn { active: true },
            Request::LaunchApp {
                app: "dev.orca.not-installed".into(),
                open: vec![],
            },
        ] {
            let failure = driver.handle(request, &output).unwrap_err();
            assert_eq!(failure.kind, ErrorKind::Failed);
            assert_eq!(failure.message, "send hello first");
        }
        let mismatch = driver
            .handle(
                Request::Hello {
                    protocol: PROTOCOL_VERSION + 1,
                },
                &output,
            )
            .unwrap_err();
        assert_eq!(mismatch.kind, ErrorKind::VersionMismatch);
        assert!(
            crate::session_lock::acquire(&path).is_ok(),
            "rejected handshake releases its lock"
        );
        let failure = driver
            .handle(Request::Turn { active: true }, &output)
            .unwrap_err();
        assert_eq!(failure.message, "send hello first");
        assert_eq!(
            driver.handle(Request::Shutdown, &output).unwrap(),
            Reply::Done
        );
    }
}

#[cfg(test)]
#[test]
fn stdin_stop_rejects_buffered_work_after_a_successful_hello() {
    let directory = tempfile::tempdir().unwrap();
    let mut driver = MacDriver::new(directory.path().join("lock"));
    let output = LineWriter::new(std::io::sink());
    driver
        .handle(
            Request::Hello {
                protocol: PROTOCOL_VERSION,
            },
            &output,
        )
        .unwrap();
    driver.stop_signal().stop();
    let failure = driver
        .handle(
            Request::Observe(ObserveParams {
                app: "dev.orca.not-installed".into(),
                window: None,
                query: None,
                max_elements: 300,
                include_screenshot: false,
            }),
            &output,
        )
        .unwrap_err();
    assert_eq!(failure.kind, ErrorKind::Failed);
    assert_eq!(
        failure.message,
        "The driver is stopping because stdin closed"
    );
    assert!(
        driver.aborted.load(Ordering::SeqCst),
        "in-flight HID delivery must be aborted too"
    );
    driver.handle(Request::Shutdown, &output).unwrap();
}

#[cfg(test)]
#[test]
fn request_entry_checks_closed_stdin_before_starting_buffered_work() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let (input, mut sender) = std::io::pipe().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut driver = MacDriver::new(directory.path().join("lock"));
    driver.stdin_fd = Some(input.as_raw_fd());
    let output = LineWriter::new(std::io::sink());
    driver
        .handle(
            Request::Hello {
                protocol: PROTOCOL_VERSION,
            },
            &output,
        )
        .unwrap();
    sender.write_all(b"buffered request\n").unwrap();
    drop(sender);
    let failure = driver
        .handle(
            Request::ResolveApp {
                app: "dev.orca.not-installed".into(),
                allow_not_running: false,
            },
            &output,
        )
        .unwrap_err();
    assert_eq!(
        failure.message,
        "The driver is stopping because stdin closed"
    );
    assert!(driver.stopping.load(Ordering::SeqCst));
}
