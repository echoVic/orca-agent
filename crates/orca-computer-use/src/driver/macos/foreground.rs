//! Short foreground actions always restore the application and pointer they borrowed.
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGError, CGEvent, CGEventSource, CGEventSourceStateID, CGEventType, CGWarpMouseCursorPosition,
};

use super::{
    actions,
    ax::AxElement,
    error,
    events::HidEvents,
    tree::{self, ElementTable, SecurityScan},
};
use crate::keys::{KeyStroke, parse_combo};
use crate::protocol::{ActParams, Action, Button, DriverError, ErrorKind, Target};

pub(super) fn check_abort(aborted: &AtomicBool) -> Result<(), DriverError> {
    if aborted.load(Ordering::SeqCst) {
        Err(error(ErrorKind::Failed, "the turn was stopped by Esc"))
    } else {
        Ok(())
    }
}

pub(super) fn frontmost_pid() -> Option<i32> {
    NSWorkspace::new()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}

fn wait_frontmost(pid: i32, timeout: Duration) -> Result<(), DriverError> {
    let deadline = Instant::now() + timeout;
    while frontmost_pid() != Some(pid) {
        if Instant::now() >= deadline {
            return Err(error(ErrorKind::Failed, "The app did not become frontmost"));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn activate(application: &NSRunningApplication, timeout: Duration) -> Result<(), DriverError> {
    if !application.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows) {
        return Err(error(ErrorKind::Failed, "Could not activate the app"));
    }
    wait_frontmost(application.processIdentifier(), timeout)
}

struct Restore {
    application: Retained<NSRunningApplication>,
    pointer: CGPoint,
    restored: bool,
    minimized_window: Option<AxElement>,
}

impl Restore {
    fn finish(&mut self) -> Result<(), DriverError> {
        // Attempt every restore, including the target's minimized state.
        let minimized = self
            .minimized_window
            .as_ref()
            .map(|window| window.set_bool("AXMinimized", true))
            .transpose();
        let app = activate(&self.application, crate::timing::RESTORE_ACTIVATION_LIMIT);
        let warped = CGWarpMouseCursorPosition(self.pointer);
        let pointer = if warped == CGError::Success {
            CGEvent::new(None)
                .ok_or_else(|| error(ErrorKind::Failed, "Could not read the restored pointer"))
                .and_then(|event| {
                    let point = CGEvent::location(Some(&event));
                    if (point.x - self.pointer.x).abs() <= 1.0
                        && (point.y - self.pointer.y).abs() <= 1.0
                    {
                        Ok(())
                    } else {
                        Err(error(ErrorKind::Failed, "The pointer was not restored"))
                    }
                })
        } else {
            Err(error(
                ErrorKind::Failed,
                format!("Could not restore the pointer: {warped:?}"),
            ))
        };
        // Explicit finish attempted every restore; Drop is only the unwind guard.
        self.restored = true;
        let mut failures = vec![];
        if let Err(failure) = minimized {
            failures.push(format!(
                "The target window was not minimized again: {}",
                failure.message
            ));
        }
        if let Err(failure) = app {
            failures.push(format!(
                "The previous app was not restored: {}",
                failure.message
            ));
        }
        if let Err(failure) = pointer {
            failures.push(format!("The pointer was not restored: {}", failure.message));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(error(ErrorKind::Failed, failures.join("; ")))
        }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.finish();
        }
    }
}

fn point(
    table: &ElementTable,
    application: &AxElement,
    target: Target,
    window: &AxElement,
) -> Result<CGPoint, DriverError> {
    let (x, y) = match target {
        Target::Point { point } => table.geometry.to_screen(point).ok_or_else(|| {
            error(
                ErrorKind::Failed,
                "The point is outside the observed window",
            )
        })?,
        Target::Element { .. } => {
            let element = actions::target_element(table, application, target)?;
            let point = element_click_point(&element, window)?;
            (point.x, point.y)
        }
    };
    Ok(CGPoint::new(x, y))
}

fn check_window(window: &AxElement, table: &ElementTable) -> Result<(), DriverError> {
    if window
        .position_size()?
        .into_iter()
        .zip(table.window_frame)
        .all(|(a, b)| a.is_finite() && (a - b).abs() <= 1.0)
    {
        Ok(())
    } else {
        Err(actions::stale())
    }
}

fn preserve_foreground_type_selection(was_focused: bool, still_focused: bool) -> bool {
    was_focused && still_focused
}

#[cfg(test)]
#[test]
fn foreground_type_selection_collapses_when_focus_was_lost_before_the_click() {
    assert!(!preserve_foreground_type_selection(true, false));
}

#[cfg(test)]
#[test]
fn foreground_type_selection_collapses_when_background_just_focused_the_target() {
    assert!(!preserve_foreground_type_selection(false, true));
}

#[cfg(test)]
#[test]
fn foreground_type_selection_is_preserved_only_with_original_and_current_focus() {
    assert!(preserve_foreground_type_selection(true, true));
    assert!(!preserve_foreground_type_selection(false, false));
}

/// Waits until the user has been idle for half a second, within the action's deadline.
fn wait_for_user_idle(aborted: &AtomicBool, action_deadline: Instant) -> Result<(), DriverError> {
    let deadline = (Instant::now() + crate::timing::IDLE_WAIT_LIMIT).min(action_deadline);
    loop {
        check_abort(aborted)?;
        if Instant::now() >= action_deadline {
            return Err(error(
                ErrorKind::Failed,
                "The foreground action timed out; observe again",
            ));
        }
        if CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::HIDSystemState,
            CGEventType(u32::MAX),
        ) >= 0.5
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(error(
                ErrorKind::UserActive,
                "the user is typing or moving the mouse; try again in a moment",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Sends an app-level shortcut (Cmd+N while only a system Open panel shows) to
/// an app with no window, then restores the previous app and pointer.
pub(super) fn run_app_shortcut(
    application: &AxElement,
    app_name: &str,
    stroke: KeyStroke,
    aborted: &AtomicBool,
    action_deadline: Instant,
) -> Result<String, DriverError> {
    let pid = application.pid()?;
    wait_for_user_idle(aborted, action_deadline)?;
    let old = NSWorkspace::new()
        .frontmostApplication()
        .ok_or_else(|| error(ErrorKind::Failed, "Could not read the frontmost app"))?;
    let name = old
        .localizedName()
        .map(|name| name.to_string())
        .unwrap_or_else(|| old.processIdentifier().to_string());
    let pointer_event =
        CGEvent::new(None).ok_or_else(|| error(ErrorKind::Failed, "Could not read the pointer"))?;
    let mut restore = Restore {
        application: old,
        pointer: CGEvent::location(Some(&pointer_event)),
        restored: false,
        minimized_window: None,
    };
    let result = (|| {
        check_abort(aborted)?;
        let target = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        activate(&target, crate::timing::ACTIVATION_WAIT_LIMIT)?;
        check_abort(aborted)?;
        thread::sleep(crate::timing::ACTIVATION_SETTLE);
        check_abort(aborted)?;
        // A window that appeared meanwhile has an unobserved focus: observe it first.
        if !application.elements("AXWindows")?.is_empty() {
            return Err(error(
                ErrorKind::Stale,
                format!("{app_name} now has a window; observe it before sending keys"),
            ));
        }
        let mut hid = HidEvents::new_until(pid, aborted, action_deadline)?;
        let sent = hid.key(stroke);
        hid.release();
        sent
    })();
    let restored = restore.finish();
    finish_foreground(result, restored, &name)
}

pub(super) fn run(
    type_target_was_focused: bool,
    application: &AxElement,
    window: &AxElement,
    table: &ElementTable,
    params: &ActParams,
    aborted: &AtomicBool,
    action_deadline: Instant,
) -> Result<String, DriverError> {
    let pid = application.pid()?;
    wait_for_user_idle(aborted, action_deadline)?;
    // The user may have moved the window while we waited for idle.
    check_window(window, table)?;
    let enters_text = match &params.action {
        Action::Type { .. } => true,
        Action::Key { keys, .. } => parse_combo(keys)
            .map_err(|m| error(ErrorKind::Failed, m))?
            .enters_text(),
        _ => false,
    };
    let target_element = keyboard_target(&params.action)
        .map(|t| actions::target_element(table, application, t))
        .transpose()?;
    if let Action::Type { text, .. } = &params.action {
        crate::type_text::validate_controls(text).map_err(|m| error(ErrorKind::Failed, m))?;
        if let Some(element) = target_element.as_ref() {
            actions::check_type_recipient(element, text)?;
        }
    }
    // A known secure target requires approval before even the focus click.
    if enters_text
        && !params.secure_approved
        && target_element
            .as_ref()
            .map(AxElement::is_secure)
            .transpose()?
            .unwrap_or(false)
    {
        return Err(error(
            ErrorKind::NeedsSecureApproval,
            "Entering text into a password field requires approval",
        ));
    }
    let old = NSWorkspace::new()
        .frontmostApplication()
        .ok_or_else(|| error(ErrorKind::Failed, "Could not read the frontmost app"))?;
    let name = old
        .localizedName()
        .map(|name| name.to_string())
        .unwrap_or_else(|| old.processIdentifier().to_string());
    let pointer_event =
        CGEvent::new(None).ok_or_else(|| error(ErrorKind::Failed, "Could not read the pointer"))?;
    let mut restore = Restore {
        application: old,
        pointer: CGEvent::location(Some(&pointer_event)),
        restored: false,
        minimized_window: window
            .attribute_bool("AXMinimized")?
            .unwrap_or(false)
            .then(|| window.clone()),
    };
    let mut unfocused_key = false;
    let mut raise_note: Option<&'static str> = None;
    let result = (|| {
        check_abort(aborted)?;
        let target = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        if restore.minimized_window.is_some() {
            window.set_bool("AXMinimized", false)?;
        }
        activate(&target, crate::timing::ACTIVATION_WAIT_LIMIT)?;
        check_abort(aborted)?;
        let app_name = target
            .localizedName()
            .map(|name| name.to_string())
            .unwrap_or_else(|| pid.to_string());
        raise_note = raise_with_fallback(
            || window.perform("AXRaise"),
            || window_in_front(window, application),
            || thread::sleep(Duration::from_millis(150)),
            &app_name,
        )?;
        check_abort(aborted)?;
        // Activation/raise can precede AppKit's key-window transition.
        // Keep this pause short and check abort again before any HID post.
        thread::sleep(crate::timing::ACTIVATION_SETTLE);
        check_window(window, table)?;
        check_abort(aborted)?;
        let location = target_location(params)
            .map(|target| point(table, application, target, window))
            .transpose()?;
        check_abort(aborted)?;
        let mut hid = HidEvents::new_until(pid, aborted, action_deadline)?;
        let result = (|| {
            match &params.action {
                Action::Click {
                    button,
                    count,
                    modifiers,
                    ..
                } => {
                    let mut keys = modifiers.clone();
                    keys.push("a".into());
                    let flags = parse_combo(&keys)
                        .map_err(|message| error(ErrorKind::Failed, message))?
                        .modifiers;
                    if !matches!(count, 1 | 2) {
                        return Err(error(ErrorKind::Failed, "Click count must be 1 or 2"));
                    }
                    hid.click(location.unwrap(), *button, *count, flags)?;
                }
                Action::Scroll {
                    direction, amount, ..
                } => hid.scroll(location.unwrap(), *direction, *amount)?,
                Action::Type { text, .. } => {
                    super::events::check_foreground_text_budget(text, action_deadline)?;
                    let mut focus_at_click = None;
                    if let Some(point) = location {
                        // AX may already have focused a sheet field whose system hit
                        // test is owned by its view service. Deliver to that exact
                        // keyboard recipient without an unnecessary pointer click.
                        let already_focused =
                            if matches!(target_location(params), Some(Target::Element { .. })) {
                                actions::keyboard_target_is_focused(
                                    target_element.as_ref(),
                                    |target| actions::keyboard_recipient(application, target),
                                )?
                            } else {
                                false
                            };
                        focus_at_click = Some(already_focused);
                        if !already_focused {
                            hid.click(point, Button::Left, 1, Default::default())?;
                        }
                    }
                    // Activation and the click can change focus after the
                    // preflight check. Inspect the actual keyboard recipient
                    // immediately before delivering any foreground text.
                    let focused = wait_for_focus(application, aborted)?.ok_or_else(|| {
                        error(ErrorKind::Failed, "The app has no focused element")
                    })?;
                    actions::check_foreground_secure_approval(
                        application,
                        target_element.as_ref(),
                        params.secure_approved,
                    )?;
                    if matches!(target_location(params), Some(Target::Element { .. })) {
                        let still_focused = match focus_at_click {
                            Some(focused) => focused,
                            None => actions::keyboard_target_is_focused(
                                target_element.as_ref(),
                                |target| actions::keyboard_recipient(application, target),
                            )?,
                        };
                        if !preserve_foreground_type_selection(
                            type_target_was_focused,
                            still_focused,
                        ) {
                            focused.collapse_selection_to_end();
                        }
                        actions::check_foreground_secure_approval(
                            application,
                            target_element.as_ref(),
                            params.secure_approved,
                        )?;
                    }
                    actions::check_type_recipient(&focused, text)?;
                    hid.text(text, || {
                        actions::verify_type_recipient(application, &focused, true)
                    })?;
                }
                Action::Key { keys, .. } => {
                    if let Some(point) = location {
                        // AX may already have focused a sheet field whose system hit
                        // test is owned by its view service. Deliver to that exact
                        // keyboard recipient without an unnecessary pointer click.
                        let already_focused =
                            if matches!(target_location(params), Some(Target::Element { .. })) {
                                actions::keyboard_target_is_focused(
                                    target_element.as_ref(),
                                    |target| actions::keyboard_recipient(application, target),
                                )?
                            } else {
                                false
                            };
                        if !already_focused {
                            hid.click(point, Button::Left, 1, Default::default())?;
                        }
                    }
                    let stroke =
                        parse_combo(keys).map_err(|message| error(ErrorKind::Failed, message))?;
                    if stroke.enters_text() {
                        if !params.secure_approved
                            && wait_for_focus(application, aborted)?.is_none()
                        {
                            if matches!(
                                keyboard_target(&params.action),
                                Some(Target::Element { .. })
                            ) {
                                return Err(error(
                                    ErrorKind::Failed,
                                    "The app has no focused element",
                                ));
                            }
                            let app_focus = application.attribute_element("AXFocusedUIElement")?;
                            let system_focus = AxElement::system_wide()?
                                .attribute_element("AXFocusedUIElement")?;
                            if !allows_unfocused_key(
                                app_focus.is_some(),
                                system_focus.is_some(),
                                tree::scan_security(Some(window), action_deadline),
                            ) {
                                return Err(error(
                                    ErrorKind::Failed,
                                    "The app has no focused element",
                                ));
                            }
                            unfocused_key = true;
                        }
                        if !unfocused_key {
                            actions::check_foreground_secure_approval(
                                application,
                                target_element.as_ref(),
                                params.secure_approved,
                            )?;
                        }
                    }
                    hid.key(stroke)?;
                }
            }
            Ok(())
        })();
        hid.release();
        result
    })();
    let restored = restore.finish();
    // Preserve successful delivery, and disclose any restoration failure.
    finish_foreground(result, restored, &name).map(|note| {
        let note = if unfocused_key {
            format!("{note}; sent key to an app that reports no focused element")
        } else {
            note
        };
        match raise_note {
            Some(raise) => format!("{note}; {raise}"),
            None => note,
        }
    })
}

pub(super) const ALREADY_IN_FRONT: &str = "window already in front (raise unsupported)";

/// System Settings refuses AXRaise (-25205) even for its frontmost window. A
/// failed raise is acceptable only for a window that is already in front;
/// otherwise retry once, then fail. Every later foreground check still runs.
pub(super) fn raise_with_fallback(
    mut raise: impl FnMut() -> Result<(), DriverError>,
    mut in_front: impl FnMut() -> bool,
    mut pause: impl FnMut(),
    app_name: &str,
) -> Result<Option<&'static str>, DriverError> {
    match raise() {
        Ok(()) => return Ok(None),
        Err(failure) if failure.kind == ErrorKind::Stale => return Err(failure),
        Err(_) => {}
    }
    if in_front() {
        return Ok(Some(ALREADY_IN_FRONT));
    }
    pause();
    let failure = match raise() {
        Ok(()) => return Ok(None),
        Err(retry) if retry.kind == ErrorKind::Stale => return Err(retry),
        Err(retry) => retry,
    };
    if in_front() {
        return Ok(Some(ALREADY_IN_FRONT));
    }
    Err(error(
        ErrorKind::Failed,
        format!(
            "Could not bring {app_name}'s window to the front ({}). Call observe again; if it keeps failing, tell the user.",
            failure.message.trim_start_matches("Accessibility ")
        ),
    ))
}

fn window_in_front(window: &AxElement, application: &AxElement) -> bool {
    window.attribute_bool("AXMain").ok().flatten() == Some(true)
        || application
            .attribute_element("AXFocusedWindow")
            .ok()
            .flatten()
            .is_some_and(|focused| focused == *window)
}
fn finish_foreground(
    result: Result<(), DriverError>,
    restored: Result<(), DriverError>,
    name: &str,
) -> Result<String, DriverError> {
    match result {
        Ok(()) => match restored {
            Ok(()) => Ok(format!("used the foreground (switched back to {name})")),
            Err(failure) => Ok(format!("used the foreground; {}", failure.message)),
        },
        Err(mut failure) => {
            if let Err(restoration) = restored {
                failure
                    .message
                    .push_str(&format!("; {}", restoration.message));
            }
            Err(failure)
        }
    }
}
#[cfg(test)]
#[test]
fn successful_actions_keep_success_when_restoration_fails() {
    for failure in ["previous app was not restored", "pointer was not restored"] {
        let note =
            finish_foreground(Ok(()), Err(error(ErrorKind::Failed, failure)), "Finder").unwrap();
        assert!(note.contains(failure));
    }
    let note = finish_foreground(Ok(()), Ok(()), "Finder").unwrap();
    assert!(note.contains("switched back to Finder"));
    assert!(
        finish_foreground(
            Err(error(ErrorKind::Failed, "action failed")),
            Ok(()),
            "Finder"
        )
        .is_err()
    );
}

fn wait_for_focus(
    application: &AxElement,
    aborted: &AtomicBool,
) -> Result<Option<AxElement>, DriverError> {
    let focus_deadline = Instant::now() + crate::timing::FOCUS_WAIT_LIMIT;
    loop {
        check_abort(aborted)?;
        if let Some(element) = application.attribute_element("AXFocusedUIElement")? {
            return Ok(Some(element));
        }
        if Instant::now() >= focus_deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn allows_unfocused_key(
    app_focus_present: bool,
    system_focus_present: bool,
    scan: SecurityScan,
) -> bool {
    !app_focus_present && !system_focus_present && scan == SecurityScan::Clean
}

#[cfg(test)]
#[test]
fn unfocused_keys_require_both_focuses_absent_and_a_fresh_clean_scan() {
    for app_focus in [false, true] {
        for system_focus in [false, true] {
            for (scan, safe) in [
                (SecurityScan::Clean, true),
                (SecurityScan::SecurePresent, false),
                (SecurityScan::Incomplete, false),
            ] {
                let expected = match (app_focus, system_focus) {
                    (false, false) => safe,
                    _ => false,
                };
                assert_eq!(
                    allows_unfocused_key(app_focus, system_focus, scan),
                    expected,
                    "app focus: {app_focus}, system focus: {system_focus}, scan: {scan:?}"
                );
            }
        }
    }
}
fn keyboard_target(action: &Action) -> Option<Target> {
    match action {
        Action::Type { target, .. } => *target,
        Action::Key { target, .. } => *target,
        _ => None,
    }
}

#[cfg(test)]
#[test]
fn non_text_keyboard_targets_are_retained_for_the_already_focused_check() {
    let target = Some(Target::Element { element: 1 });
    for key in ["left", "return", "tab", "space", "a"] {
        assert_eq!(
            keyboard_target(&Action::Key {
                keys: vec![key.into()],
                target
            }),
            target,
            "{key}"
        );
    }
    assert_eq!(
        keyboard_target(&Action::Type {
            text: "text".into(),
            target
        }),
        target
    );
}

fn target_location(params: &ActParams) -> Option<Target> {
    match &params.action {
        Action::Click { target, .. } | Action::Scroll { target, .. } => Some(*target),
        Action::Type { target, .. } | Action::Key { target, .. } => *target,
    }
}
fn element_click_point(element: &AxElement, window: &AxElement) -> Result<CGPoint, DriverError> {
    if let Some(point) = visible_center(element.position_size()?, window.position_size()?) {
        return Ok(point);
    }
    // ScrollToVisible can be unsupported. Re-read either way, never click through.
    let _ = element.perform("AXScrollToVisible");
    visible_center(element.position_size()?, window.position_size()?).ok_or_else(|| {
        error(
            ErrorKind::Failed,
            "The element is outside the visible window; observe again",
        )
    })
}
fn visible_center(frame: [f64; 4], window: [f64; 4]) -> Option<CGPoint> {
    if frame.into_iter().chain(window).any(|v| !v.is_finite())
        || frame[2] <= 0.0
        || frame[3] <= 0.0
        || window[2] <= 0.0
        || window[3] <= 0.0
    {
        return None;
    }
    let left = frame[0].max(window[0]);
    let top = frame[1].max(window[1]);
    let right = (frame[0] + frame[2]).min(window[0] + window[2]);
    let bottom = (frame[1] + frame[3]).min(window[1] + window[3]);
    (right > left && bottom > top).then(|| CGPoint::new((left + right) / 2.0, (top + bottom) / 2.0))
}

#[cfg(test)]
#[test]
fn click_centers_stay_in_the_visible_intersection() {
    let window = [100.0, 100.0, 200.0, 100.0];
    assert!(visible_center([110.0, 300.0, 30.0, 20.0], window).is_none());
    assert!(visible_center([110.0, 110.0, 0.0, 20.0], window).is_none());
    assert_eq!(
        visible_center([110.0, 180.0, 100.0, 200.0], window),
        Some(CGPoint::new(160.0, 190.0))
    );
    assert_eq!(
        visible_center([110.0, 110.0, 100.0, 20.0], window),
        Some(CGPoint::new(160.0, 120.0))
    );
}
#[cfg(feature = "test-utils")]
pub fn test_element_click_point(
    element: objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>,
    window: objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>,
) -> Result<CGPoint, DriverError> {
    let element = AxElement::from_owned(element)?;
    let window = AxElement::from_owned(window)?;
    element_click_point(&element, &window)
}

#[cfg(test)]
mod raise_tests {
    use super::*;
    use std::cell::Cell;

    fn failed() -> Result<(), DriverError> {
        Err(error(
            ErrorKind::Failed,
            "Accessibility AXRaise failed: AXError(-25205)",
        ))
    }

    #[test]
    fn a_raised_window_needs_no_fallback() {
        let raises = Cell::new(0);
        let note = raise_with_fallback(
            || {
                raises.set(raises.get() + 1);
                Ok(())
            },
            || false,
            || {},
            "System Settings",
        )
        .unwrap();
        assert_eq!((note, raises.get()), (None, 1));
    }

    #[test]
    fn an_unsupported_raise_is_fine_for_a_window_already_in_front() {
        let raises = Cell::new(0);
        let note = raise_with_fallback(
            || {
                raises.set(raises.get() + 1);
                failed()
            },
            || true,
            || {},
            "System Settings",
        )
        .unwrap();
        assert_eq!((note, raises.get()), (Some(ALREADY_IN_FRONT), 1));
    }

    #[test]
    fn a_failed_raise_is_retried_once_after_a_pause() {
        let raises = Cell::new(0);
        let paused = Cell::new(false);
        let note = raise_with_fallback(
            || {
                raises.set(raises.get() + 1);
                if raises.get() == 1 { failed() } else { Ok(()) }
            },
            || false,
            || paused.set(true),
            "System Settings",
        )
        .unwrap();
        assert_eq!((note, raises.get(), paused.get()), (None, 2, true));
    }

    #[test]
    fn two_failed_raises_fail_unless_the_window_reached_the_front() {
        let checks = Cell::new(0);
        let note = raise_with_fallback(
            failed,
            || {
                checks.set(checks.get() + 1);
                checks.get() == 2
            },
            || {},
            "System Settings",
        )
        .unwrap();
        assert_eq!(note, Some(ALREADY_IN_FRONT));
        let error = raise_with_fallback(failed, || false, || {}, "System Settings").unwrap_err();
        assert_eq!(
            error.message,
            "Could not bring System Settings's window to the front (AXRaise failed: AXError(-25205)). Call observe again; if it keeps failing, tell the user."
        );
    }

    #[test]
    fn a_stale_window_is_not_retried() {
        let raises = Cell::new(0);
        let error = raise_with_fallback(
            || {
                raises.set(raises.get() + 1);
                Err(actions::stale())
            },
            || true,
            || {},
            "System Settings",
        )
        .unwrap_err();
        assert_eq!((error.kind, raises.get()), (ErrorKind::Stale, 1));
    }
}
