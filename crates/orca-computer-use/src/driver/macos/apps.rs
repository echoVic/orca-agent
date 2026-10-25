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

use super::windows::{AxWindowFacts, CaptureWindowFacts, usable_windows};
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

fn windows(timeout: Duration) -> Result<HashMap<i32, Vec<CaptureWindowFacts>>, DriverError> {
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
                    let mut windows: HashMap<i32, Vec<CaptureWindowFacts>> = HashMap::new();
                    for window in content.windows() {
                        if window.windowLayer() == 0
                            && let Some(app) = window.owningApplication()
                        {
                            let frame = window.frame();
                            windows
                                .entry(app.processID())
                                .or_default()
                                .push(CaptureWindowFacts {
                                    id: window.windowID(),
                                    title: window
                                        .title()
                                        .map(|title| title.to_string())
                                        .unwrap_or_default(),
                                    frame: [
                                        frame.origin.x,
                                        frame.origin.y,
                                        frame.size.width,
                                        frame.size.height,
                                    ],
                                    on_screen: window.isOnScreen(),
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

fn merge_apps(
    running: Vec<AppInfo>,
    mut capture: HashMap<i32, Vec<CaptureWindowFacts>>,
    mut ax_facts: impl FnMut(i32) -> Option<Vec<AxWindowFacts>>,
) -> Vec<AppInfo> {
    let mut apps: Vec<_> = running
        .into_iter()
        .map(|mut app| {
            app.windows = app
                .pid
                .and_then(|pid| capture.remove(&pid).map(|windows| (pid, windows)))
                .map(|(pid, windows)| usable_windows(&windows, ax_facts(pid).as_deref()))
                .unwrap_or_default();
            app
        })
        .collect();
    apps.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id).then(a.pid.cmp(&b.pid)));
    apps
}

/// The whole listing's accessibility budget, and each app's share of it. An
/// app that does not answer in time keeps the visible document-sized rule.
const LIST_AX_BUDGET: Duration = Duration::from_secs(2);
const APP_AX_TIMEOUT_SECS: f32 = 0.25;

fn running_apps() -> Vec<AppInfo> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|app| app.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter_map(|app| running_info(&app))
        .collect()
}

fn list_with_timeout(timeout: Duration, verify_windows: bool) -> Result<Vec<AppInfo>, DriverError> {
    let capture = windows(timeout)?;
    let running = running_apps();
    let ax_deadline = Instant::now() + LIST_AX_BUDGET.min(timeout);
    Ok(merge_apps(running, capture, |pid| {
        verify_windows
            .then(|| ax_window_facts(pid, ax_deadline))
            .flatten()
    }))
}

/// This app's share of what is left of the listing budget; None once it is spent.
fn share_of(deadline: Instant) -> Option<f32> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    (!remaining.is_zero()).then(|| remaining.as_secs_f32().min(APP_AX_TIMEOUT_SECS))
}

/// Reads windows while the budget lasts; an app that runs out falls back to
/// the visible document-sized rule (None) instead of extending the listing.
fn read_within<W>(
    deadline: Instant,
    windows: impl IntoIterator<Item = W>,
    mut read: impl FnMut(W, f32) -> Option<AxWindowFacts>,
) -> Option<Vec<AxWindowFacts>> {
    windows
        .into_iter()
        .map(|window| read(window, share_of(deadline)?))
        .collect()
}

fn with_timeout(element: &AxElement, seconds: f32) -> Option<()> {
    (unsafe { element.0.set_messaging_timeout(seconds) } == AXError::Success).then_some(())
}

fn ax_window_facts(pid: i32, deadline: Instant) -> Option<Vec<AxWindowFacts>> {
    let application = AxElement::application(pid).ok()?;
    with_timeout(&application, share_of(deadline)?)?;
    let windows = application.elements("AXWindows").ok()?;
    read_within(deadline, windows, |window, timeout| {
        // Every read gets only what is left, so slow answers cannot add up.
        with_timeout(&window, timeout)?;
        let id = window.window_id();
        with_timeout(&window, share_of(deadline)?)?;
        let frame = window.position_size().ok()?;
        with_timeout(&window, share_of(deadline)?)?;
        let subrole = window.attribute_string("AXSubrole").ok().flatten();
        with_timeout(&window, share_of(deadline)?)?;
        let minimized = window
            .attribute_bool("AXMinimized")
            .ok()
            .flatten()
            .unwrap_or(false);
        Some(AxWindowFacts {
            id,
            frame,
            subrole,
            minimized,
        })
    })
}

fn wait_for_launch(
    receiver: mpsc::Receiver<Result<i32, DriverError>>,
    deadline: Instant,
) -> Result<i32, DriverError> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| error(ErrorKind::Failed, "App launch timed out"))?
}

/// Resolution only needs identities, so it skips the accessibility check.
pub(super) fn list() -> Result<Vec<AppInfo>, DriverError> {
    list_with_timeout(Duration::from_secs(30), false)
}

/// What `list_apps` shows the model: windows observe and act can use.
pub(super) fn list_verified() -> Result<Vec<AppInfo>, DriverError> {
    list_with_timeout(Duration::from_secs(30), true)
}

/// Every regular app's windows by id and title, unfiltered and without
/// accessibility reads: who owns which window, off-screen and minimized ones
/// included, for telling the user's windows from Orca's.
pub(super) fn snapshot() -> Result<Vec<AppInfo>, DriverError> {
    let capture = windows(Duration::from_secs(10))?;
    Ok(snapshot_apps(running_apps(), capture))
}

fn snapshot_apps(
    running: Vec<AppInfo>,
    mut capture: HashMap<i32, Vec<CaptureWindowFacts>>,
) -> Vec<AppInfo> {
    let mut apps: Vec<_> = running
        .into_iter()
        .map(|mut app| {
            app.windows = app
                .pid
                .and_then(|pid| capture.remove(&pid))
                .unwrap_or_default()
                .into_iter()
                .map(|window| WindowInfo {
                    id: window.id,
                    title: window.title,
                    minimized: false,
                })
                .collect();
            app
        })
        .collect();
    apps.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id).then(a.pid.cmp(&b.pid)));
    apps
}

/// Observe/act receive an already approved bundle ID; names cannot widen it.
pub(super) fn resolve_approved(bundle_id: &str) -> Result<AppInfo, DriverError> {
    resolve_approved_from_apps(bundle_id, list()?)
}

fn resolve_approved_from_apps(bundle_id: &str, apps: Vec<AppInfo>) -> Result<AppInfo, DriverError> {
    let candidates: Vec<_> = apps
        .iter()
        .filter(|app| app.bundle_id == bundle_id)
        .collect();
    select_instance(
        bundle_id,
        candidates,
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier()),
    )?
    .cloned()
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
    if input != app.bundle_id {
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
    if !looks_like_bundle_id(name) && app.name.to_lowercase() == name.to_lowercase() {
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
    Ok(bundle_name_matches(
        &bundle,
        name,
        !looks_like_bundle_id(name),
    ))
}

fn bundle_folder_matches(bundle: &NSBundle, name: &str) -> bool {
    bundle.bundleURL().path().is_some_and(|path| {
        std::path::Path::new(&path.to_string())
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.to_lowercase() == name.to_lowercase())
    })
}

fn bundle_name_matches(bundle: &NSBundle, name: &str, include_display_name: bool) -> bool {
    // infoDictionary is intentionally non-localized.
    let raw_name = bundle
        .infoDictionary()
        .and_then(|info| info.objectForKey(&NSString::from_str("CFBundleName")))
        .and_then(|value| value.downcast::<NSString>().ok())
        .map(|value| value.to_string());
    let raw_name = raw_name.filter(|raw| !raw.is_empty());
    let display_name = info_string(bundle, "CFBundleDisplayName").filter(|name| !name.is_empty());
    if raw_name
        .as_ref()
        .is_some_and(|raw| raw.to_lowercase() == name.to_lowercase())
        || (include_display_name
            && display_name
                .as_ref()
                .is_some_and(|display| display.to_lowercase() == name.to_lowercase()))
    {
        return true;
    }
    if looks_like_bundle_id(name) && (raw_name.is_some() || display_name.is_some()) {
        return false;
    }
    bundle_folder_matches(bundle, name)
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

/// Resolve exact IDs before folded IDs; folded variants are distinct identities.
fn select_identity<'a>(
    name: &str,
    apps: impl IntoIterator<Item = &'a AppInfo>,
) -> Result<Option<&'a AppInfo>, DriverError> {
    let apps: Vec<_> = apps.into_iter().collect();
    let exact: Vec<_> = apps
        .iter()
        .copied()
        .filter(|app| app.bundle_id == name)
        .collect();
    let candidates = if exact.is_empty() {
        apps.into_iter()
            .filter(|app| app.bundle_id.eq_ignore_ascii_case(name))
            .collect()
    } else {
        exact
    };
    select_instance(
        name,
        candidates,
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier()),
    )
}

fn select_instance<'a>(
    name: &str,
    candidates: Vec<&'a AppInfo>,
    frontmost: Option<i32>,
) -> Result<Option<&'a AppInfo>, DriverError> {
    let Some(first) = candidates.first().copied() else {
        return Ok(None);
    };
    if candidates
        .iter()
        .any(|app| app.bundle_id != first.bundle_id)
    {
        return Err(DriverError {
            kind: ErrorKind::AppAmbiguous,
            message: format!("More than one app matches {name}; use its exact bundle ID"),
            candidates: candidates.into_iter().cloned().collect(),
        });
    }
    Ok(Some(
        candidates
            .into_iter()
            .find(|app| app.pid.is_some() && app.pid == frontmost)
            .unwrap_or(first),
    ))
}

fn installed_bundles(roots: &[std::path::PathBuf]) -> Vec<(AppInfo, Retained<NSBundle>)> {
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
            candidates.push((info, bundle));
        }
    }
    candidates.sort_by(|(a, _), (b, _)| a.bundle_id.cmp(&b.bundle_id));
    candidates.dedup_by(|a, b| a.0.bundle_id == b.0.bundle_id);
    candidates
}

fn find_installed_bundle(
    name: &str,
    roots: &[std::path::PathBuf],
) -> Result<Option<Retained<NSBundle>>, DriverError> {
    let mut candidates = installed_bundles(roots);
    if let Some(identity) = select_identity(name, candidates.iter().map(|(app, _)| app))? {
        let id = identity.bundle_id.clone();
        return Ok(candidates
            .into_iter()
            .find(|(app, _)| app.bundle_id == id)
            .map(|(_, bundle)| bundle));
    }
    candidates.retain(|(_, bundle)| bundle_name_matches(bundle, name, true));
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

fn installed_identities(name: &str, roots: &[std::path::PathBuf]) -> Vec<AppInfo> {
    let mut candidates: Vec<_> = installed_bundles(roots)
        .into_iter()
        .map(|(info, _)| info)
        .filter(|info| info.bundle_id.eq_ignore_ascii_case(name))
        .collect();
    if let Some(url) = NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(name))
        && let Some(bundle) = NSBundle::bundleWithURL(&url)
        && let Some(info) = installed_info(&bundle, name)
        && info.bundle_id.eq_ignore_ascii_case(name)
        && !candidates
            .iter()
            .any(|candidate| candidate.bundle_id == info.bundle_id)
    {
        candidates.push(info);
    }
    candidates
}

fn resolve_with_apps(
    name: &str,
    allow_not_running: bool,
    apps: Vec<AppInfo>,
    roots: &[std::path::PathBuf],
) -> Result<AppInfo, DriverError> {
    if let Some(app) = select_identity(name, apps.iter().filter(|app| app.bundle_id == name))? {
        return Ok(app.clone());
    }
    // Installed exact identities must win over a case-folded running impostor,
    // including resolve(..., false): a quit installed app cannot become another app.
    let mut identities = apps.clone();
    for installed in installed_identities(name, roots) {
        if !identities
            .iter()
            .any(|app| app.bundle_id == installed.bundle_id)
        {
            identities.push(installed);
        }
    }
    if let Some(app) = select_identity(name, &identities)? {
        if app.pid.is_some() || allow_not_running {
            return Ok(app.clone());
        }
        return Err(error(
            ErrorKind::AppNotFound,
            format!("App is installed but not running: {}", app.bundle_id),
        ));
    }
    let mut candidates = vec![];
    for app in apps {
        if running_name_matches(&app, name, roots)? {
            candidates.push(app);
        }
    }
    if let Some(app) = select_instance(
        name,
        candidates.iter().collect(),
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier()),
    )? {
        return Ok(app.clone());
    }
    if allow_not_running
        && let Some(bundle) = find_installed_bundle(name, roots)?
        && let Some(info) = installed_info(&bundle, name)
    {
        return Ok(info); // The bundle's own name was checked by the finder.
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

fn find_exact_launch_bundle(
    bundle_id: &str,
    roots: &[std::path::PathBuf],
    running_url: Option<Retained<NSURL>>,
) -> Result<Option<Retained<NSBundle>>, DriverError> {
    if let Some(bundle) = running_url
        .and_then(|url| NSBundle::bundleWithURL(&url))
        .filter(|bundle| {
            bundle
                .bundleIdentifier()
                .is_some_and(|id| id.to_string() == bundle_id)
        })
    {
        return Ok(Some(bundle));
    }
    if let Some((_, bundle)) = installed_bundles(roots)
        .into_iter()
        .find(|(info, _)| info.bundle_id == bundle_id)
    {
        return Ok(Some(bundle));
    }
    Ok(NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))
        .and_then(|url| NSBundle::bundleWithURL(&url))
        .filter(|bundle| {
            bundle
                .bundleIdentifier()
                .is_some_and(|id| id.to_string() == bundle_id)
        }))
}

pub(super) fn launch(name: &str, open: &[String]) -> Result<AppInfo, DriverError> {
    let deadline = Instant::now() + Duration::from_secs(60);
    // This is an approved canonical ID. Resolve names only before approval.
    let workspace = NSWorkspace::sharedWorkspace();
    let is_approved = |app: &NSRunningApplication| {
        app.bundleIdentifier()
            .is_some_and(|id| id.to_string() == name)
    };
    let running_url = workspace
        .frontmostApplication()
        .filter(|app| is_approved(app))
        .or_else(|| {
            workspace
                .runningApplications()
                .iter()
                .find(|app| is_approved(app))
        })
        .and_then(|app| app.bundleURL());
    let bundle =
        find_exact_launch_bundle(name, &application_roots(), running_url)?.ok_or_else(|| {
            error(
                ErrorKind::AppNotFound,
                format!("Approved app not found: {name}"),
            )
        })?;
    let app = installed_info(&bundle, name).ok_or_else(|| {
        error(
            ErrorKind::AppNotFound,
            "The approved application has no bundle identity",
        )
    })?;
    require_bundle_identity(name, &app)?;
    let url = bundle.bundleURL();
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
        if let Some(app) = list_with_timeout(remaining, false)?
            .into_iter()
            .find(|candidate| {
                candidate.bundle_id == app.bundle_id && candidate.pid == Some(launched_pid)
            })
        {
            if wait_for_accessibility_window(launched_pid, deadline) {
                // The first capture listing can precede the now-readable window.
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !remaining.is_zero()
                    && let Some(refreshed) =
                        list_with_timeout(remaining, true)?
                            .into_iter()
                            .find(|candidate| {
                                candidate.pid == Some(launched_pid)
                                    && candidate.bundle_id == app.bundle_id
                            })
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
    fn a_window_snapshot_keeps_every_window_unfiltered() {
        let app = |pid: i32| AppInfo {
            bundle_id: format!("dev.orca.{pid}"),
            name: pid.to_string(),
            pid: Some(pid),
            category: None,
            windows: vec![],
        };
        let facts = |id: u32, frame: [f64; 4], on_screen: bool| CaptureWindowFacts {
            id,
            title: format!("w{id}"),
            frame,
            on_screen,
        };
        // A document, an off-screen (minimized) one and a small helper all count.
        let snapshot = snapshot_apps(
            vec![app(2), app(1)],
            HashMap::from([(
                2,
                vec![
                    facts(9, [0.0, 0.0, 800.0, 600.0], true),
                    facts(10, [0.0, 0.0, 800.0, 600.0], false),
                    facts(11, [0.0, 0.0, 20.0, 10.0], true),
                ],
            )]),
        );
        assert_eq!(
            snapshot.iter().map(|app| app.pid).collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
        assert!(snapshot[0].windows.is_empty());
        assert_eq!(
            snapshot[1]
                .windows
                .iter()
                .map(|window| window.id)
                .collect::<Vec<_>>(),
            vec![9, 10, 11]
        );
    }

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

    fn running_fixture(id: &str, name: &str, pid: i32) -> AppInfo {
        AppInfo {
            bundle_id: id.into(),
            name: name.into(),
            pid: Some(pid),
            category: None,
            windows: vec![],
        }
    }

    #[test]
    fn part6_launch_prefers_the_running_instances_own_exact_bundle() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let installed = temp.path().join("Applications");
        let running = temp.path().join("RunningCopy");
        application_fixture(&installed, "TextEdit", "dev.orca.launch-copy");
        application_fixture(&running, "TextEdit", "dev.orca.launch-copy");
        let running_path = running.join("TextEdit.app");
        let running_url =
            NSURL::fileURLWithPath(&NSString::from_str(&running_path.to_string_lossy()));
        let bundle = find_exact_launch_bundle(
            "dev.orca.launch-copy",
            std::slice::from_ref(&installed),
            Some(running_url.clone()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            bundle.bundleURL().path().unwrap().to_string(),
            running_path.to_string_lossy(),
            "an installed second copy must not displace the running bundle"
        );
        assert!(
            find_exact_launch_bundle("dev.orca.launch-copy", &[], Some(running_url))
                .unwrap()
                .is_some(),
            "a running copy need not live in the application roots"
        );
        // A running URL with a different canonical ID cannot substitute for approval.
        application_fixture(temp.path(), "WrongCopy", "DEV.ORCA.LAUNCH-COPY");
        let wrong_url = NSURL::fileURLWithPath(&NSString::from_str(
            &temp.path().join("WrongCopy.app").to_string_lossy(),
        ));
        for url in [None, Some(wrong_url)] {
            let bundle = find_exact_launch_bundle(
                "dev.orca.launch-copy",
                std::slice::from_ref(&installed),
                url,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                bundle.bundleURL().path().unwrap().to_string(),
                installed.join("TextEdit.app").to_string_lossy()
            );
        }
    }

    #[test]
    fn part5_launch_binding_refuses_name_and_case_fallbacks() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "TextEdit", "dev.orca.launch");
        for input in ["TextEdit", "DEV.ORCA.LAUNCH", "dev.orca.missing"] {
            assert!(
                find_exact_launch_bundle(input, &[temp.path().into()], None)
                    .unwrap()
                    .is_none(),
                "launch must not re-resolve {input}"
            );
        }
        let exact = find_exact_launch_bundle("dev.orca.launch", &[temp.path().into()], None)
            .unwrap()
            .unwrap();
        assert_eq!(
            exact.bundleIdentifier().unwrap().to_string(),
            "dev.orca.launch"
        );
    }

    #[test]
    fn folder_only_alias_is_allowed_when_neither_bundle_name_is_declared() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "dev.orca.alias", "dev.orca.running");
        let plist = temp.path().join("dev.orca.alias.app/Contents/Info.plist");
        let body = std::fs::read_to_string(&plist)
            .unwrap()
            .replace("<key>CFBundleName</key><string>TextEdit</string>", "")
            .replace(
                "<key>CFBundleDisplayName</key><string>Localized Editor</string>",
                "",
            );
        std::fs::write(plist, body).unwrap();
        let running = running_fixture("dev.orca.running", "Localized UI Name", 42);
        assert_eq!(
            resolve_with_apps(
                "dev.orca.alias",
                false,
                vec![running],
                &[temp.path().into()]
            )
            .unwrap()
            .bundle_id,
            "dev.orca.running"
        );
        assert_eq!(
            resolve_with_apps("dev.orca.alias", true, vec![], &[temp.path().into()])
                .unwrap()
                .bundle_id,
            "dev.orca.running"
        );
    }

    #[test]
    fn part6_dotted_folder_alias_cannot_hide_a_declared_display_name() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "dev.orca.alias", "dev.orca.display-name");
        let plist = temp.path().join("dev.orca.alias.app/Contents/Info.plist");
        let text = std::fs::read_to_string(&plist)
            .unwrap()
            .replace("<key>CFBundleName</key><string>TextEdit</string>", "");
        std::fs::write(plist, text).unwrap();
        let running = running_fixture("dev.orca.display-name", "dev.orca.alias", 42);
        for (allow, apps) in [(true, vec![]), (false, vec![running])] {
            assert_eq!(
                resolve_with_apps("dev.orca.alias", allow, apps, &[temp.path().into()])
                    .unwrap_err()
                    .kind,
                ErrorKind::AppNotFound,
                "a declared CFBundleDisplayName rules out a dotted folder alias"
            );
        }
    }

    #[test]
    fn part5_renamed_folder_alias_is_rejected_on_both_running_and_installed_paths() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "dev.orca.alias", "dev.orca.running");
        let running = running_fixture("dev.orca.running", "dev.orca.alias", 42);
        for (allow, apps) in [(false, vec![running]), (true, vec![])] {
            assert_eq!(
                resolve_with_apps("dev.orca.alias", allow, apps, &[temp.path().into()])
                    .unwrap_err()
                    .kind,
                ErrorKind::AppNotFound
            );
        }
    }

    #[test]
    fn part5_same_id_instances_choose_frontmost_or_first() {
        let first = running_fixture("dev.orca.multi", "Multi", 42);
        let second = running_fixture("dev.orca.multi", "Multi", 43);
        assert_eq!(
            select_instance("dev.orca.multi", vec![&first, &second], Some(43)).unwrap(),
            Some(&second)
        );
        assert_eq!(
            select_instance("dev.orca.multi", vec![&first, &second], Some(99)).unwrap(),
            Some(&first)
        );
        assert_eq!(
            select_instance("dev.orca.multi", vec![&first, &second], None).unwrap(),
            Some(&first)
        );
        let variant = running_fixture("DEV.ORCA.MULTI", "Variant", 44);
        assert_eq!(
            select_instance("Dev.Orca.Multi", vec![&first, &variant], Some(44))
                .unwrap_err()
                .kind,
            ErrorKind::AppAmbiguous
        );
    }

    #[test]
    fn part5_same_id_instances_resolve_as_one_app() {
        let first = running_fixture("dev.orca.multi", "Multi", 42);
        let second = running_fixture("dev.orca.multi", "Multi", 43);
        assert_eq!(
            resolve_with_apps(
                "dev.orca.multi",
                false,
                vec![first.clone(), second.clone()],
                &[]
            )
            .unwrap()
            .bundle_id,
            "dev.orca.multi"
        );
        assert_eq!(
            resolve_with_apps(
                "DEV.ORCA.MULTI",
                false,
                vec![first.clone(), second.clone()],
                &[]
            )
            .unwrap()
            .bundle_id,
            "dev.orca.multi"
        );
        assert_eq!(
            resolve_with_apps("Multi", false, vec![first, second], &[])
                .unwrap()
                .bundle_id,
            "dev.orca.multi"
        );
    }

    #[test]
    fn part4_identity_launch_guard_keeps_the_selected_canonical_id() {
        let impostor = running_fixture("com.apple.passwords", "Passwords", 42);
        assert_eq!(
            require_bundle_identity("com.apple.Passwords", &impostor)
                .unwrap_err()
                .kind,
            ErrorKind::AppNotFound
        );
        let canonical = running_fixture("com.apple.Passwords", "Passwords", 43);
        assert!(require_bundle_identity("com.apple.Passwords", &canonical).is_ok());
    }

    #[test]
    fn part4_dotted_inputs_cannot_use_a_running_localized_name_alone() {
        let impostor = running_fixture("dev.orca.impostor", "dev.orca.wanted", 42);
        assert_eq!(
            resolve_with_apps("dev.orca.wanted", false, vec![impostor.clone()], &[])
                .unwrap_err()
                .kind,
            ErrorKind::AppNotFound
        );
        assert_eq!(
            resolve_with_apps("dev.orca.wanted", true, vec![impostor], &[])
                .unwrap_err()
                .kind,
            ErrorKind::AppNotFound
        );
    }

    #[test]
    fn part4_dotted_display_name_is_not_a_declared_running_alias() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "OtherFolder", "dev.orca.impostor");
        let plist = temp.path().join("OtherFolder.app/Contents/Info.plist");
        std::fs::write(
            &plist,
            std::fs::read_to_string(&plist)
                .unwrap()
                .replace("Localized Editor", "dev.orca.wanted"),
        )
        .unwrap();
        let impostor = running_fixture("dev.orca.impostor", "Localized UI Name", 42);
        assert_eq!(
            resolve_with_apps(
                "dev.orca.wanted",
                false,
                vec![impostor],
                &[temp.path().into()]
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppNotFound
        );
    }

    #[test]
    fn part4_dotted_running_folder_alias_belongs_to_the_exact_same_bundle() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "dev.orca.alias", "dev.orca.running");
        let declared = running_fixture("dev.orca.running", "Localized UI Name", 42);
        assert_eq!(
            resolve_with_apps(
                "dev.orca.alias",
                false,
                vec![declared],
                &[temp.path().into()]
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppNotFound
        );
        let variant = running_fixture("DEV.ORCA.RUNNING", "dev.orca.alias", 43);
        assert_eq!(
            resolve_with_apps(
                "dev.orca.alias",
                false,
                vec![variant],
                &[temp.path().into()]
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppNotFound
        );
    }

    #[test]
    fn part4_dotted_installed_id_never_aliases_a_different_running_app() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "Installed", "dev.orca.wanted");
        application_fixture(temp.path(), "dev.orca.wanted", "dev.orca.other");
        let other = running_fixture("dev.orca.other", "dev.orca.wanted", 42);
        assert_eq!(
            resolve_with_apps(
                "dev.orca.wanted",
                false,
                vec![other.clone()],
                &[temp.path().into()]
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppNotFound
        );
        assert_eq!(
            resolve_with_apps("dev.orca.wanted", true, vec![other], &[temp.path().into()])
                .unwrap()
                .bundle_id,
            "dev.orca.wanted"
        );
    }

    #[test]
    fn part4_identity_running_exact_match_precedes_case_folded_match() {
        let impostor = running_fixture("com.apple.passwords", "Passwords", 42);
        let canonical = running_fixture("com.apple.Passwords", "Passwords", 43);
        assert_eq!(
            resolve_with_apps(
                "com.apple.Passwords",
                false,
                vec![impostor, canonical.clone()],
                &[]
            )
            .unwrap(),
            canonical
        );
    }

    #[test]
    fn part4_identity_approved_id_never_accepts_a_case_variant() {
        let impostor = running_fixture("com.apple.passwords", "Passwords", 42);
        assert_eq!(
            resolve_approved_from_apps("com.apple.Passwords", vec![impostor])
                .unwrap_err()
                .kind,
            ErrorKind::AppNotFound
        );
        let canonical = running_fixture("com.apple.Passwords", "Passwords", 43);
        assert_eq!(
            resolve_approved_from_apps("com.apple.Passwords", vec![canonical.clone()]).unwrap(),
            canonical
        );
    }

    #[test]
    fn part4_identity_installed_canonical_passwords_cannot_be_lowered_by_a_running_impostor() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "Passwords", "com.apple.Passwords");
        let impostor = running_fixture("com.apple.passwords", "Passwords", 42);
        let resolved = resolve_with_apps(
            "com.apple.Passwords",
            true,
            vec![impostor.clone()],
            &[temp.path().into()],
        )
        .unwrap();
        assert_eq!(resolved.bundle_id, "com.apple.Passwords");
        assert_eq!(
            crate::apps::risk_tier(&resolved.bundle_id, resolved.category.as_deref(), None),
            crate::apps::RiskTier::Forbidden
        );
        assert_eq!(
            resolve_with_apps(
                "com.apple.Passwords",
                false,
                vec![impostor],
                &[temp.path().into()]
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppNotFound
        );
    }

    #[test]
    fn part4_identity_running_case_variants_are_ambiguous_without_an_exact_match() {
        let lower = running_fixture("dev.orca.case-app", "Lower", 42);
        let upper = running_fixture("dev.orca.Case-App", "Upper", 43);
        assert_eq!(
            resolve_with_apps(
                "DEV.ORCA.CASE-APP",
                false,
                vec![lower.clone(), upper.clone()],
                &[]
            )
            .unwrap_err()
            .kind,
            ErrorKind::AppAmbiguous
        );
        assert_eq!(
            resolve_with_apps("dev.orca.Case-App", false, vec![lower, upper.clone()], &[]).unwrap(),
            upper
        );
    }

    #[test]
    fn part4_identity_installed_case_variants_are_ambiguous_without_an_exact_match() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "Lower", "dev.orca.case-app");
        application_fixture(temp.path(), "Upper", "dev.orca.Case-App");
        assert_eq!(
            resolve_with_apps("DEV.ORCA.CASE-APP", true, vec![], &[temp.path().into()])
                .unwrap_err()
                .kind,
            ErrorKind::AppAmbiguous
        );
        for id in ["dev.orca.case-app", "dev.orca.Case-App"] {
            assert_eq!(
                resolve_with_apps(id, true, vec![], &[temp.path().into()])
                    .unwrap()
                    .bundle_id,
                id
            );
        }
    }

    #[test]
    fn bundle_id_resolution_is_case_insensitive_and_retains_canonical_identity() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "CaseApp", "dev.orca.case-app");
        let running = AppInfo {
            bundle_id: "dev.orca.case-app".into(),
            name: "CaseApp".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        assert_eq!(
            resolve_with_apps("DEV.ORCA.CASE-APP", false, vec![running.clone()], &[]).unwrap(),
            running
        );
        assert_eq!(
            resolve_with_apps("DEV.ORCA.CASE-APP", true, vec![], &[temp.path().into()])
                .unwrap()
                .bundle_id,
            "dev.orca.case-app"
        );
        assert!(require_bundle_identity(&running.bundle_id, &running).is_ok());
    }

    #[test]
    fn dotted_app_names_resolve_only_when_the_bundle_owns_the_name() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        application_fixture(temp.path(), "zoom.us", "dev.orca.zoom");
        let plist = temp.path().join("zoom.us.app/Contents/Info.plist");
        let text = std::fs::read_to_string(&plist)
            .unwrap()
            .replace("<string>TextEdit</string>", "<string>zoom.us</string>");
        std::fs::write(&plist, text).unwrap();
        application_fixture(temp.path(), "OtherFolder", "dev.orca.draw");
        let plist = temp.path().join("OtherFolder.app/Contents/Info.plist");
        let text = std::fs::read_to_string(&plist)
            .unwrap()
            .replace(
                "<string>Localized Editor</string>",
                "<string>draw.io</string>",
            )
            .replace("<string>TextEdit</string>", "<string>draw.io</string>");
        std::fs::write(&plist, text).unwrap();
        for (name, id) in [("zoom.us", "dev.orca.zoom"), ("draw.io", "dev.orca.draw")] {
            assert_eq!(
                resolve_with_apps(name, true, vec![], &[temp.path().into()])
                    .unwrap()
                    .bundle_id,
                id
            );
            let running = AppInfo {
                bundle_id: id.into(),
                name: name.into(),
                pid: Some(42),
                category: None,
                windows: vec![],
            };
            assert_eq!(
                resolve_with_apps(name, false, vec![running.clone()], &[temp.path().into()])
                    .unwrap(),
                running
            );
        }
        let localized = AppInfo {
            bundle_id: "dev.orca.zoom".into(),
            name: "本地名称".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        assert_eq!(
            resolve_with_apps(
                "zoom.us",
                false,
                vec![localized.clone()],
                &[temp.path().into()]
            )
            .unwrap(),
            localized
        );
        application_fixture(temp.path(), "folder.only", "dev.orca.folder");
        let plist = temp.path().join("folder.only.app/Contents/Info.plist");
        let text = std::fs::read_to_string(&plist)
            .unwrap()
            .replace("<key>CFBundleName</key><string>TextEdit</string>", "")
            .replace(
                "<key>CFBundleDisplayName</key><string>Localized Editor</string>",
                "",
            );
        std::fs::write(&plist, text).unwrap();
        assert_eq!(
            resolve_with_apps("folder.only", true, vec![], &[temp.path().into()])
                .unwrap()
                .bundle_id,
            "dev.orca.folder"
        );
        // Exact IDs still win over another app which owns that display name.
        let real = AppInfo {
            bundle_id: "zoom.us".into(),
            name: "Real identity".into(),
            pid: Some(43),
            category: None,
            windows: vec![],
        };
        let named = AppInfo {
            bundle_id: "dev.orca.zoom".into(),
            name: "zoom.us".into(),
            pid: Some(42),
            category: None,
            windows: vec![],
        };
        assert_eq!(
            resolve_with_apps("zoom.us", false, vec![named, real.clone()], &[]).unwrap(),
            real
        );
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
    fn a_bundle_id_input_cannot_resolve_a_differently_named_app_by_a_renamed_folder() {
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
        let facts = CaptureWindowFacts {
            id: 7,
            title: "Visible".into(),
            frame: [0.0, 0.0, 800.0, 600.0],
            on_screen: true,
        };
        let mut visible = app.clone();
        visible.pid = Some(43);
        let merged = merge_apps(
            vec![app.clone(), visible],
            HashMap::from([(43, vec![facts])]),
            |_| None,
        );
        assert_eq!(merged.iter().find(|info| info.pid == Some(42)), Some(&app));
        assert_eq!(
            merged
                .iter()
                .find(|info| info.pid == Some(43))
                .unwrap()
                .windows,
            vec![WindowInfo {
                id: 7,
                title: "Visible".into(),
                minimized: false
            }]
        );
    }

    #[test]
    fn merge_asks_accessibility_only_for_apps_with_capture_windows() {
        let app = |pid: i32| AppInfo {
            bundle_id: format!("dev.orca.{pid}"),
            name: pid.to_string(),
            pid: Some(pid),
            category: None,
            windows: vec![],
        };
        let mut asked = vec![];
        let facts = CaptureWindowFacts {
            id: 9,
            title: "Doc".into(),
            frame: [0.0, 0.0, 800.0, 600.0],
            on_screen: true,
        };
        let merged = merge_apps(
            vec![app(1), app(2)],
            HashMap::from([(2, vec![facts])]),
            |pid| {
                asked.push(pid);
                Some(vec![AxWindowFacts {
                    id: None,
                    frame: [0.0, 0.0, 800.0, 600.0],
                    subrole: Some("AXStandardWindow".into()),
                    minimized: false,
                }])
            },
        );
        assert_eq!(asked, vec![2]);
        assert_eq!(
            merged
                .iter()
                .find(|info| info.pid == Some(2))
                .unwrap()
                .windows
                .len(),
            1
        );
    }

    #[test]
    fn slow_window_reads_stop_at_the_listing_deadline() {
        // Five windows whose reads each take 30 ms, with 50 ms left: the app is
        // abandoned at the deadline instead of finishing all five reads.
        let started = Instant::now();
        let deadline = started + Duration::from_millis(50);
        let mut reads = 0;
        let facts = read_within(deadline, 0..5, |_, timeout| {
            assert!(timeout <= APP_AX_TIMEOUT_SECS);
            reads += 1;
            std::thread::sleep(Duration::from_millis(30));
            Some(AxWindowFacts {
                id: None,
                frame: [0.0, 0.0, 800.0, 600.0],
                subrole: None,
                minimized: false,
            })
        });
        assert_eq!(facts, None);
        assert!(reads <= 2, "{reads} reads");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn ax_window_facts_skip_apps_once_the_listing_budget_is_spent() {
        // A spent deadline returns before any accessibility call, so a hung app
        // cannot extend list_apps.
        let started = Instant::now();
        assert_eq!(
            ax_window_facts(
                std::process::id() as i32,
                started - Duration::from_millis(1)
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_millis(50));
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

/// Native acceptance uses real running processes with owned application roots.
#[cfg(feature = "test-utils")]
pub fn test_resolve_with_running_from_roots(
    name: &str,
    allow_not_running: bool,
    roots: &[std::path::PathBuf],
) -> Result<AppInfo, DriverError> {
    resolve_with_apps(name, allow_not_running, list()?, roots)
}
