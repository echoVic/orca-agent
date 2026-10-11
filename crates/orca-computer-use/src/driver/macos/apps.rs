use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_foundation::{NSArray, NSBundle, NSError, NSString, NSURL};
use objc2_screen_capture_kit::SCShareableContent;

use super::error;
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

struct Desktop {
    windows: HashMap<i32, Vec<WindowInfo>>,
    pids: Vec<i32>,
}

fn windows(timeout: Duration) -> Result<Desktop, DriverError> {
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
                    Ok(Desktop {
                        windows,
                        pids: content
                            .applications()
                            .iter()
                            .map(|app| app.processID())
                            .collect(),
                    })
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

fn list_with_timeout(timeout: Duration) -> Result<Vec<AppInfo>, DriverError> {
    let mut desktop = windows(timeout)?;
    let mut running: HashMap<i32, Retained<NSRunningApplication>> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .map(|app| (app.processIdentifier(), app))
        .collect();
    // NSWorkspace caches its array until the main AppKit loop runs. The stdio
    // server and libtest are blocking hosts, so use ScreenCaptureKit's fresh
    // process snapshot to refresh the NSRunningApplication objects by PID.
    for pid in &desktop.pids {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(*pid) {
            running.insert(*pid, app);
        }
    }
    let mut apps: Vec<_> = running
        .into_values()
        .filter(|app| desktop.pids.contains(&app.processIdentifier()))
        .filter(|app| app.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter_map(|app| {
            let mut info = running_info(&app)?;
            info.windows = desktop
                .windows
                .remove(&app.processIdentifier())
                .unwrap_or_default();
            Some(info)
        })
        .collect();
    apps.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id).then(a.pid.cmp(&b.pid)));
    Ok(apps)
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
    // LaunchServices may deliver completion on AppKit's main loop. Poll the
    // fresh application snapshot as well, so a blocking stdio host can launch.
    let mut launched_pid = None;
    loop {
        match receiver.try_recv() {
            Ok(Err(error)) => return Err(error),
            Ok(Ok(pid)) => launched_pid = Some(pid),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if launched_pid.is_some() => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(error(ErrorKind::Failed, "App launch disconnected"));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error(ErrorKind::Failed, "App launch timed out"));
        }
        if let Some(app) = list_with_timeout(remaining)?.into_iter().find(|candidate| {
            candidate.bundle_id == app.bundle_id
                && launched_pid.is_none_or(|pid| candidate.pid == Some(pid))
        }) && (open.is_empty() || !app.windows.is_empty())
        {
            return Ok(app);
        }
        thread::sleep(Duration::from_millis(100).min(remaining));
    }
}
