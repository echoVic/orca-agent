use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_application_services::AXError;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_foundation::{NSArray, NSBundle, NSError, NSString, NSURL};
use objc2_screen_capture_kit::SCShareableContent;

use super::{ax::AxElement, error};
use crate::protocol::{AppInfo, DriverError, ErrorKind, WindowInfo};

fn info_string(bundle: &NSBundle, key: &str) -> Option<String> {
    bundle
        .objectForInfoDictionaryKey(&NSString::from_str(key))?
        .downcast::<NSString>()
        .ok()
        .map(|value| value.to_string())
}

fn running_info(app: &NSRunningApplication) -> Option<AppInfo> {
    let bundle = app
        .bundleURL()
        .and_then(|url| NSBundle::bundleWithURL(&url));
    Some(AppInfo {
        bundle_id: app.bundleIdentifier()?.to_string(),
        name: app
            .localizedName()
            .map(|name| name.to_string())
            .unwrap_or_default(),
        pid: Some(app.processIdentifier()),
        category: bundle
            .as_ref()
            .and_then(|bundle| info_string(bundle, "LSApplicationCategoryType")),
        windows: vec![],
    })
}

fn windows(timeout: Duration) -> Result<HashMap<i32, Vec<WindowInfo>>, DriverError> {
    if !CGPreflightScreenCaptureAccess() {
        return Err(error(
            ErrorKind::PermissionMissing,
            "Enable Screen Recording for your terminal in System Settings > Privacy & Security; you may need to restart the terminal",
        ));
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, failure: *mut NSError| {
            let result = unsafe {
                if let Some(failure) = failure.as_ref() {
                    Err(error(
                        ErrorKind::Failed,
                        format!("Could not list windows: {}", failure.localizedDescription()),
                    ))
                } else if let Some(content) = content.as_ref() {
                    let mut windows: HashMap<i32, Vec<WindowInfo>> = HashMap::new();
                    for window in content.windows() {
                        if window.windowLayer() == 0
                            && let Some(app) = window.owningApplication()
                        {
                            windows
                                .entry(app.processID())
                                .or_default()
                                .push(WindowInfo {
                                    id: window.windowID(),
                                    title: window
                                        .title()
                                        .map(|title| title.to_string())
                                        .unwrap_or_default(),
                                });
                        }
                    }
                    for values in windows.values_mut() {
                        values.sort_by_key(|window| window.id);
                    }
                    Ok(windows)
                } else {
                    Err(error(
                        ErrorKind::Failed,
                        "Window listing returned no content",
                    ))
                }
            };
            let _ = sender.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&completion);
    }
    receiver
        .recv_timeout(timeout)
        .map_err(|_| error(ErrorKind::Failed, "Window listing timed out"))?
}

fn merge_apps(running: Vec<AppInfo>, mut windows: HashMap<i32, Vec<WindowInfo>>) -> Vec<AppInfo> {
    let mut apps: Vec<_> = running
        .into_iter()
        .map(|mut app| {
            app.windows = app
                .pid
                .and_then(|pid| windows.remove(&pid))
                .unwrap_or_default();
            app
        })
        .collect();
    apps.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id).then(a.pid.cmp(&b.pid)));
    apps
}

fn list_with_timeout(timeout: Duration) -> Result<Vec<AppInfo>, DriverError> {
    let windows = windows(timeout)?;
    let running = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|app| app.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter_map(|app| running_info(&app))
        .collect();
    Ok(merge_apps(running, windows))
}

fn wait_for_launch(
    receiver: mpsc::Receiver<Result<i32, DriverError>>,
    deadline: Instant,
) -> Result<i32, DriverError> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| error(ErrorKind::Failed, "App launch timed out"))?
}

pub(super) fn list() -> Result<Vec<AppInfo>, DriverError> {
    list_with_timeout(Duration::from_secs(30))
}

pub(super) fn resolve(name: &str, allow_not_running: bool) -> Result<AppInfo, DriverError> {
    let apps = list()?;
    if let Some(app) = apps.iter().find(|app| app.bundle_id == name) {
        return Ok(app.clone());
    }
    let candidates: Vec<_> = apps
        .into_iter()
        .filter(|app| app.name.to_lowercase() == name.to_lowercase())
        .collect();
    match candidates.as_slice() {
        [app] => return Ok(app.clone()),
        [] => {}
        _ => {
            return Err(DriverError {
                kind: ErrorKind::AppAmbiguous,
                message: format!("More than one app is named {name}; use a bundle ID"),
                candidates,
            });
        }
    }
    if allow_not_running {
        let workspace = NSWorkspace::sharedWorkspace();
        if let Some(url) =
            workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(name))
            && let Some(bundle) = NSBundle::bundleWithURL(&url)
        {
            return Ok(AppInfo {
                bundle_id: bundle
                    .bundleIdentifier()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| name.into()),
                name: info_string(&bundle, "CFBundleDisplayName")
                    .or_else(|| info_string(&bundle, "CFBundleName"))
                    .unwrap_or_else(|| name.into()),
                pid: None,
                category: info_string(&bundle, "LSApplicationCategoryType"),
                windows: vec![],
            });
        }
    }
    Err(error(
        ErrorKind::AppNotFound,
        format!("App not found: {name}"),
    ))
}

fn wait_for_accessibility_window(pid: i32, launch_deadline: Instant) -> bool {
    let deadline = launch_deadline.min(Instant::now() + Duration::from_secs(10));
    let Ok(application) = AxElement::application(pid) else {
        return false;
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return false;
    }
    // Enable the same accessibility support as observe before polling Electron.
    // This handle belongs only to launch; observe gets its usual 2 s timeout.
    if unsafe {
        application
            .0
            .set_messaging_timeout(remaining.as_secs_f32().min(2.0))
    } != AXError::Success
    {
        return false;
    }
    let _ = application.set_bool("AXManualAccessibility", true);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        // Bound even the final native call by the remaining launch/window budget.
        if unsafe {
            application
                .0
                .set_messaging_timeout(remaining.as_secs_f32().min(2.0))
        } == AXError::Success
            && application
                .elements("AXWindows")
                .is_ok_and(|windows| !windows.is_empty())
        {
            return true;
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(super) fn launch(name: &str, open: &[String]) -> Result<AppInfo, DriverError> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let app = resolve(name, true)?;
    let workspace = NSWorkspace::sharedWorkspace();
    let url = app
        .pid
        .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        .and_then(|app| app.bundleURL())
        .or_else(|| {
            workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(&app.bundle_id))
        })
        .ok_or_else(|| error(ErrorKind::AppNotFound, format!("App not found: {name}")))?;
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setPromptsUserIfNeeded(false);
    configuration.setAddsToRecentItems(false);
    let (sender, receiver) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |app: *mut NSRunningApplication, failure: *mut NSError| {
            let result = unsafe {
                if let Some(failure) = failure.as_ref() {
                    Err(error(
                        ErrorKind::Failed,
                        format!("Could not launch app: {}", failure.localizedDescription()),
                    ))
                } else if let Some(app) = app.as_ref() {
                    Ok(app.processIdentifier())
                } else {
                    Err(error(
                        ErrorKind::Failed,
                        "App launch returned no application",
                    ))
                }
            };
            let _ = sender.send(result);
        },
    );
    if open.is_empty() {
        workspace.openApplicationAtURL_configuration_completionHandler(
            &url,
            &configuration,
            Some(&completion),
        );
    } else {
        let urls: Vec<Retained<NSURL>> = open
            .iter()
            .map(|value| {
                let text = NSString::from_str(value);
                match NSURL::URLWithString(&text) {
                    Some(url) if url.scheme().is_some() => Ok(url),
                    _ if !value.contains("://") => Ok(NSURL::fileURLWithPath(&text)),
                    _ => Err(error(ErrorKind::Failed, format!("Invalid URL: {value}"))),
                }
            })
            .collect::<Result<_, _>>()?;
        workspace.openURLs_withApplicationAtURL_configuration_completionHandler(
            &NSArray::from_retained_slice(&urls),
            &url,
            &configuration,
            Some(&completion),
        );
    }
    let launched_pid = wait_for_launch(receiver, deadline)?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error(ErrorKind::Failed, "App launch timed out"));
        }
        if let Some(app) = list_with_timeout(remaining)?.into_iter().find(|candidate| {
            candidate.bundle_id == app.bundle_id && candidate.pid == Some(launched_pid)
        }) {
            if wait_for_accessibility_window(launched_pid, deadline) {
                // The first capture listing can precede the now-readable window.
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !remaining.is_zero()
                    && let Some(refreshed) = list_with_timeout(remaining)?
                        .into_iter()
                        .find(|candidate| candidate.pid == Some(launched_pid))
                {
                    return Ok(refreshed);
                }
            }
            // Windowless apps still launch successfully after the bounded wait.
            return Ok(app);
        }
        thread::sleep(Duration::from_millis(100).min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_ax_window_uses_the_remaining_launch_deadline() {
        let started = Instant::now();
        assert!(!wait_for_accessibility_window(
            i32::MAX,
            started + Duration::from_millis(100)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_regular_app_without_shareable_windows_stays_in_the_list() {
        let app = AppInfo {
            bundle_id: "dev.orca.windowless".into(),
            name: "Windowless".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        let window = WindowInfo {
            id: 7,
            title: "Visible".into(),
        };
        let mut visible = app.clone();
        visible.pid = Some(43);
        let merged = merge_apps(
            vec![app.clone(), visible],
            HashMap::from([(43, vec![window.clone()])]),
        );
        assert_eq!(merged.iter().find(|info| info.pid == Some(42)), Some(&app));
        assert_eq!(
            merged
                .iter()
                .find(|info| info.pid == Some(43))
                .unwrap()
                .windows,
            vec![window]
        );
    }

    #[test]
    fn a_delayed_native_launch_error_is_preserved() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let expected = error(ErrorKind::Failed, "The requested file does not exist");
        let sent = expected.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            sender.send(Err(sent)).unwrap();
        });
        assert_eq!(
            wait_for_launch(receiver, Instant::now() + Duration::from_secs(1)),
            Err(expected)
        );
        worker.join().unwrap();
    }

    #[test]
    fn native_launch_completion_must_arrive_within_the_deadline() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let failure =
            wait_for_launch(receiver, Instant::now() + Duration::from_millis(10)).unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Failed);
        assert_eq!(failure.message, "App launch timed out");
    }
}
