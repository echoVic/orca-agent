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

/// Observe/act receive an already approved bundle ID; names cannot widen it.
pub(super) fn resolve_approved(bundle_id: &str) -> Result<AppInfo, DriverError> {
    list()?
        .into_iter()
        .find(|app| app.bundle_id == bundle_id)
        .ok_or_else(|| {
            error(
                ErrorKind::AppNotFound,
                format!("App not found: {bundle_id}"),
            )
        })
}

fn looks_like_bundle_id(name: &str) -> bool {
    name.contains('.')
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

fn require_bundle_identity(input: &str, app: &AppInfo) -> Result<(), DriverError> {
    if looks_like_bundle_id(input) && input != app.bundle_id {
        return Err(error(
            ErrorKind::AppNotFound,
            format!(
                "Requested bundle ID {input:?} resolved to {:?}; refusing a different app",
                app.bundle_id
            ),
        ));
    }
    Ok(())
}

fn running_name_matches(
    app: &AppInfo,
    name: &str,
    roots: &[std::path::PathBuf],
) -> Result<bool, DriverError> {
    if app.name.to_lowercase() == name.to_lowercase() {
        return Ok(true);
    }
    let url = app
        .pid
        .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        .and_then(|running| running.bundleURL())
        .or_else(|| {
            NSWorkspace::sharedWorkspace()
                .URLForApplicationWithBundleIdentifier(&NSString::from_str(&app.bundle_id))
        });
    let bundle = match url.and_then(|url| NSBundle::bundleWithURL(&url)) {
        Some(bundle) => Some(bundle),
        None => find_installed_bundle(&app.bundle_id, roots)?,
    };
    let Some(bundle) = bundle else {
        return Ok(false);
    };
    if bundle
        .bundleIdentifier()
        .is_none_or(|id| id.to_string() != app.bundle_id)
    {
        return Ok(false);
    }
    // infoDictionary is intentionally non-localized, unlike objectForInfoDictionaryKey.
    let raw_name = bundle
        .infoDictionary()
        .and_then(|info| info.objectForKey(&NSString::from_str("CFBundleName")))
        .and_then(|value| value.downcast::<NSString>().ok())
        .map(|value| value.to_string());
    let folder_name = bundle.bundleURL().path().and_then(|path| {
        std::path::Path::new(&path.to_string())
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
    });
    let wanted = name.to_lowercase();
    Ok(raw_name
        .into_iter()
        .chain(folder_name)
        .any(|alias| alias.to_lowercase() == wanted))
}

fn application_roots() -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        "/Applications".into(),
        "/System/Applications".into(),
        "/System/Applications/Utilities".into(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(std::path::PathBuf::from(home).join("Applications"));
    }
    roots
}

fn installed_info(bundle: &NSBundle, fallback_name: &str) -> Option<AppInfo> {
    let bundle_id = bundle.bundleIdentifier()?.to_string();
    if bundle_id.is_empty() {
        return None;
    }
    Some(AppInfo {
        bundle_id,
        name: info_string(bundle, "CFBundleDisplayName")
            .or_else(|| info_string(bundle, "CFBundleName"))
            .unwrap_or_else(|| fallback_name.into()),
        pid: None,
        category: info_string(bundle, "LSApplicationCategoryType"),
        windows: vec![],
    })
}

fn find_installed_bundle(
    name: &str,
    roots: &[std::path::PathBuf],
) -> Result<Option<Retained<NSBundle>>, DriverError> {
    let wanted = name.to_lowercase();
    let mut candidates = vec![];
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir()
                || !path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("app"))
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let Some(bundle) = NSBundle::bundleWithURL(&url) else {
                continue;
            };
            let Some(info) = installed_info(&bundle, stem) else {
                continue;
            };
            if info.bundle_id == name
                || (!looks_like_bundle_id(name) && stem.to_lowercase() == wanted)
            {
                candidates.push((info, bundle));
            }
        }
    }
    // Explicit identities win over a bundle folder that happens to have that name.
    if candidates.iter().any(|(app, _)| app.bundle_id == name) {
        candidates.retain(|(app, _)| app.bundle_id == name);
    }
    candidates.sort_by(|(a, _), (b, _)| a.bundle_id.cmp(&b.bundle_id));
    candidates.dedup_by(|a, b| a.0.bundle_id == b.0.bundle_id);
    if candidates.len() > 1 {
        return Err(DriverError {
            kind: ErrorKind::AppAmbiguous,
            message: format!("More than one app is named {name}; use a bundle ID"),
            candidates: candidates.into_iter().map(|(info, _)| info).collect(),
        });
    }
    Ok(candidates.pop().map(|(_, bundle)| bundle))
}

/// Read-only native resolver seam: no app listing, launch, AX or HID delivery.
#[cfg(feature = "test-utils")]
pub fn test_resolve_app_from_roots(
    name: &str,
    allow_not_running: bool,
    roots: &[std::path::PathBuf],
) -> Result<AppInfo, DriverError> {
    resolve_with_apps(name, allow_not_running, vec![], roots)
}

pub(super) fn resolve(name: &str, allow_not_running: bool) -> Result<AppInfo, DriverError> {
    resolve_with_apps(name, allow_not_running, list()?, &application_roots())
}

fn resolve_with_apps(
    name: &str,
    allow_not_running: bool,
    apps: Vec<AppInfo>,
    roots: &[std::path::PathBuf],
) -> Result<AppInfo, DriverError> {
    if let Some(app) = apps.iter().find(|app| app.bundle_id == name) {
        return Ok(app.clone());
    }
    let mut candidates = vec![];
    if !looks_like_bundle_id(name) {
        for app in apps {
            if running_name_matches(&app, name, roots)? {
                candidates.push(app);
            }
        }
    }
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
            && let Some(info) = installed_info(&bundle, name)
        {
            require_bundle_identity(name, &info)?;
            return Ok(info);
        }
        if let Some(bundle) = find_installed_bundle(name, roots)?
            && let Some(info) = installed_info(&bundle, name)
        {
            require_bundle_identity(name, &info)?;
            return Ok(info);
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
    require_bundle_identity(name, &app)?;
    let workspace = NSWorkspace::sharedWorkspace();
    let url = app
        .pid
        .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        .and_then(|app| app.bundleURL())
        .or_else(|| {
            workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(&app.bundle_id))
        });
    let url = match url {
        Some(url) => url,
        None => find_installed_bundle(&app.bundle_id, &application_roots())?
            .map(|bundle| bundle.bundleURL())
            .ok_or_else(|| error(ErrorKind::AppNotFound, format!("App not found: {name}")))?,
    };
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setActivates(false);
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

    fn application_fixture(root: &std::path::Path, folder: &str, id: &str) {
        let contents = root.join(format!("{folder}.app/Contents"));
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleName</key><string>TextEdit</string>
<key>CFBundleDisplayName</key><string>Localized Editor</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
</dict></plist>"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn installed_name_resolution_reads_case_insensitive_bundles_in_each_application_root() {
        for folder in [
            "Applications",
            "System/Applications",
            "System/Applications/Utilities",
            "Home/Applications",
        ] {
            let temp = tempfile::tempdir_in("/tmp").unwrap();
            let root = temp.path().join(folder);
            application_fixture(&root, "TeXtEdIt", "dev.orca.test.installed.textedit");
            let roots = [temp.path().join("missing"), root];
            for name in [
                "TextEdit",
                "textedit",
                "TEXTEDIT",
                "dev.orca.test.installed.textedit",
            ] {
                let resolved = resolve_with_apps(name, true, vec![], &roots).unwrap();
                assert_eq!(resolved.bundle_id, "dev.orca.test.installed.textedit");
                assert_eq!(resolved.name, "Localized Editor");
                assert_eq!(
                    resolved.category.as_deref(),
                    Some("public.app-category.productivity")
                );
                assert_eq!(resolved.pid, None);
                assert!(resolved.windows.is_empty());
            }
            assert_eq!(
                resolve_with_apps("TextEdit", false, vec![], &roots)
                    .unwrap_err()
                    .kind,
                ErrorKind::AppNotFound
            );
        }
    }

    #[test]
    fn installed_name_resolution_preserves_running_bundle_id_and_name_precedence() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "TextEdit", "dev.orca.test.installed.textedit");
        let running = AppInfo {
            bundle_id: "dev.orca.test.running.textedit".into(),
            name: "TextEdit".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        for name in ["textedit", "dev.orca.test.running.textedit"] {
            assert_eq!(
                resolve_with_apps(name, true, vec![running.clone()], &[temp.path().into()])
                    .unwrap(),
                running
            );
        }
    }

    #[test]
    fn installed_name_resolution_does_not_invent_ids_for_invalid_or_missing_bundles() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        for (name, plist) in [
            (
                "NoId",
                "<plist version=\"1.0\"><dict><key>CFBundleName</key><string>NoId</string></dict></plist>",
            ),
            ("Invalid", "invalid plist"),
        ] {
            let contents = temp.path().join(format!("{name}.app/Contents"));
            std::fs::create_dir_all(&contents).unwrap();
            std::fs::write(contents.join("Info.plist"), plist).unwrap();
        }
        std::fs::create_dir(temp.path().join("MissingInfo.app")).unwrap();
        for name in ["NoId", "Invalid", "MissingInfo", "Absent", "../NoId"] {
            assert_eq!(
                resolve_with_apps(name, true, vec![], &[temp.path().into()])
                    .unwrap_err()
                    .kind,
                ErrorKind::AppNotFound
            );
        }
    }

    #[test]
    fn installed_name_resolution_reports_duplicate_names_instead_of_choosing_an_identity() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let roots = [
            temp.path().join("Applications"),
            temp.path().join("Home/Applications"),
        ];
        application_fixture(&roots[0], "TextEdit", "dev.orca.test.first.textedit");
        application_fixture(&roots[1], "textedit", "dev.orca.test.second.textedit");
        let error = resolve_with_apps("TEXTEDIT", true, vec![], &roots).unwrap_err();
        assert_eq!(error.kind, ErrorKind::AppAmbiguous);
        assert_eq!(
            error
                .candidates
                .iter()
                .map(|a| a.bundle_id.as_str())
                .collect::<Vec<_>>(),
            [
                "dev.orca.test.first.textedit",
                "dev.orca.test.second.textedit"
            ]
        );
        let by_id =
            resolve_with_apps("dev.orca.test.second.textedit", true, vec![], &roots).unwrap();
        assert_eq!(by_id.bundle_id, "dev.orca.test.second.textedit");
    }

    #[test]
    fn a_bundle_id_input_cannot_resolve_another_app_by_folder_or_display_name() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "com.real.app", "dev.orca.other-app");
        assert_eq!(
            resolve_with_apps("com.real.app", true, vec![], &[temp.path().into()])
                .unwrap_err()
                .kind,
            ErrorKind::AppNotFound
        );
        let impostor = AppInfo {
            bundle_id: "dev.orca.other-app".into(),
            name: "com.real.app".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        assert_eq!(
            resolve_with_apps("com.real.app", false, vec![impostor], &[])
                .unwrap_err()
                .kind,
            ErrorKind::AppNotFound
        );
        let real = AppInfo {
            bundle_id: "com.real.app".into(),
            name: "Real".into(),
            pid: Some(43),
            category: None,
            windows: vec![],
        };
        assert_eq!(
            resolve_with_apps("com.real.app", false, vec![real.clone()], &[]).unwrap(),
            real
        );
    }

    #[test]
    fn running_app_names_accept_raw_bundle_and_folder_names_as_well_as_localized_name() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "EditorFolder", "dev.orca.localized-editor");
        let app = AppInfo {
            bundle_id: "dev.orca.localized-editor".into(),
            name: "文本编辑".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        for name in [
            "TextEdit",
            "textedit",
            "EditorFolder",
            "editorfolder",
            "文本编辑",
        ] {
            assert_eq!(
                resolve_with_apps(name, false, vec![app.clone()], &[temp.path().into()]).unwrap(),
                app
            );
        }
    }

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
