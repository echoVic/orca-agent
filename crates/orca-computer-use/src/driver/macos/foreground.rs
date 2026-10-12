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

use super::{actions, ax::AxElement, error, events::HidEvents, tree::ElementTable};
use crate::keys::parse_combo;
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

pub(super) fn run(
    pid: i32,
    application: &AxElement,
    window: &AxElement,
    table: &ElementTable,
    params: &ActParams,
    aborted: &AtomicBool,
) -> Result<String, DriverError> {
    let action_deadline = Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT;
    let deadline = Instant::now() + crate::timing::IDLE_WAIT_LIMIT;
    loop {
        check_abort(aborted)?;
        if CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::HIDSystemState,
            CGEventType(u32::MAX),
        ) >= 0.5
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err(error(
                ErrorKind::UserActive,
                "the user is typing or moving the mouse; try again in a moment",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    // The user may have moved the window while we waited for idle.
    check_window(window, table)?;
    let target = match &params.action {
        Action::Click { target, .. } | Action::Scroll { target, .. } => Some(*target),
        Action::Type { target, .. } | Action::Key { target, .. } => *target,
    };
    let enters_text = match &params.action {
        Action::Type { .. } => true,
        Action::Key { keys, .. } => parse_combo(keys)
            .map_err(|m| error(ErrorKind::Failed, m))?
            .enters_text(),
        _ => false,
    };
    let target_element = if enters_text {
        target
            .map(|t| actions::target_element(table, application, t))
            .transpose()?
    } else {
        None
    };
    // A known secure target requires approval before even the focus click.
    if enters_text
        && !params.secure_approved
        && target_element
            .as_ref()
            .map(|e| e.attribute_string("AXSubrole"))
            .transpose()?
            .flatten()
            .as_deref()
            == Some("AXSecureTextField")
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
    let result = (|| {
        check_abort(aborted)?;
        let target = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        if restore.minimized_window.is_some() {
            window.set_bool("AXMinimized", false)?;
        }
        activate(&target, crate::timing::ACTIVATION_WAIT_LIMIT)?;
        check_abort(aborted)?;
        window.perform("AXRaise")?;
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
                    if let Some(point) = location {
                        hid.click(point, Button::Left, 1, Default::default())?;
                    }
                    // Activation and the click can change focus after the
                    // preflight check. Inspect the actual keyboard recipient
                    // immediately before delivering any foreground text.
                    let focused = wait_for_focus(application, aborted)?;
                    actions::check_secure_approval(
                        application,
                        target_element.as_ref(),
                        params.secure_approved,
                    )?;
                    if matches!(target_location(params), Some(Target::Element { .. })) {
                        focused.collapse_selection_to_end();
                        actions::check_secure_approval(
                            application,
                            target_element.as_ref(),
                            params.secure_approved,
                        )?;
                    }
                    hid.text(text)?;
                }
                Action::Key { keys, .. } => {
                    if let Some(point) = location {
                        hid.click(point, Button::Left, 1, Default::default())?;
                    }
                    let stroke =
                        parse_combo(keys).map_err(|message| error(ErrorKind::Failed, message))?;
                    if stroke.enters_text() {
                        if !params.secure_approved {
                            wait_for_focus(application, aborted)?;
                        }
                        actions::check_secure_approval(
                            application,
                            target_element.as_ref(),
                            params.secure_approved,
                        )?;
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
    finish_foreground(result, restored, &name)
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

fn wait_for_focus(application: &AxElement, aborted: &AtomicBool) -> Result<AxElement, DriverError> {
    let focus_deadline = Instant::now() + crate::timing::FOCUS_WAIT_LIMIT;
    loop {
        check_abort(aborted)?;
        if let Some(element) = application.attribute_element("AXFocusedUIElement")? {
            return Ok(element);
        }
        if Instant::now() >= focus_deadline {
            return Err(error(ErrorKind::Failed, "The app has no focused element"));
        }
        thread::sleep(Duration::from_millis(50));
    }
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
