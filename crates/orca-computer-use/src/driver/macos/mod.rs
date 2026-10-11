mod actions;
mod apps;
mod ax;
mod capture;
mod esc_tap;
mod events;
mod foreground;
mod permissions;
mod tree;

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
    hello_complete: bool,
    pub(crate) element_tables: HashMap<(i32, u32), ElementTable>,
    accessibility_enabled: HashSet<i32>,
}

impl MacDriver {
    pub fn new(lock_path: PathBuf) -> Self {
        Self {
            lock_path,
            lock: None,
            esc_tap: None,
            aborted: Arc::new(AtomicBool::new(false)),
            hello_complete: false,
            element_tables: HashMap::new(),
            accessibility_enabled: HashSet::new(),
        }
    }

    fn observe(&mut self, params: ObserveParams) -> Result<Observation, DriverError> {
        capture::check_unlocked()?;
        let app = apps::resolve(&params.app, false)?;
        let pid = app
            .pid
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        let application = AxElement::application(pid)?;
        if self.accessibility_enabled.insert(pid) {
            let _ = application.set_bool("AXManualAccessibility", true);
        }
        let windows = capture::windows()?;
        let ax_windows = application.elements("AXWindows")?;
        let (ax_window, window) = if let Some(id) = params.window {
            let window = windows
                .iter()
                .find(|w| w.id == id && w.pid == pid)
                .ok_or_else(|| error(ErrorKind::WindowNotFound, "Window not found for this app"))?;
            let mut matching = None;
            for candidate in ax_windows {
                if frames_match(candidate.position_size()?, window.frame) {
                    matching = Some(candidate);
                    break;
                }
            }
            (
                matching.ok_or_else(|| {
                    error(
                        ErrorKind::WindowNotFound,
                        "No accessibility window matches the capture window",
                    )
                })?,
                window,
            )
        } else {
            let window = application
                .attribute_element("AXMainWindow")?
                .or_else(|| ax_windows.into_iter().next())
                .ok_or_else(|| {
                    error(
                        ErrorKind::WindowNotFound,
                        "The app has no accessibility window",
                    )
                })?;
            let frame = window.position_size()?;
            let capture = windows
                .iter()
                .find(|w| w.pid == pid && frames_match(frame, w.frame))
                .ok_or_else(|| {
                    error(
                        ErrorKind::WindowNotFound,
                        "No capture window matches the accessibility window",
                    )
                })?;
            (window, capture)
        };
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
        let (elements, entries, omitted, no_element_tree) = tree::collect(
            &ax_window,
            geometry,
            params.query.as_deref(),
            params.max_elements,
        )?;
        let focused_secure = application
            .attribute_element("AXFocusedUIElement")?
            .map(|e| e.attribute_string("AXSubrole"))
            .transpose()?
            .flatten()
            .as_deref()
            == Some("AXSecureTextField");
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
            },
            elements,
            omitted,
            no_element_tree,
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
                apps: apps::list()?,
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

fn frames_match(ax: [f64; 4], capture: [f64; 4]) -> bool {
    ax.into_iter()
        .zip(capture)
        .all(|(a, b)| (a - b).abs() <= 2.0)
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
