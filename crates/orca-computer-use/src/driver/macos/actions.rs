use objc2_app_kit::NSRunningApplication;

use super::{
    MacDriver, apps, ax::AxElement, capture, error, events, foreground, resolve_window,
    tree::ElementTable,
};
use crate::{
    keys::parse_combo,
    protocol::{
        ActOutcome, ActParams, Action, Button, Direction, DriverError, ErrorKind, ObserveParams,
        Target,
    },
};

pub(super) fn stale() -> DriverError {
    error(
        ErrorKind::Stale,
        "the window changed since the last observe; observe again",
    )
}
fn foreground(message: impl Into<String>) -> DriverError {
    error(ErrorKind::NeedsForeground, message)
}

fn check_keyboard_recipient(
    app_pid: i32,
    target_pid: i32,
    focused_pid: i32,
    modal: bool,
) -> Result<(), DriverError> {
    if target_pid != app_pid || focused_pid != app_pid || modal {
        return Err(foreground(
            "Foreign keyboard recipients and modal sheets require the foreground",
        ));
    }
    Ok(())
}

enum ModalStep<T> {
    Root(bool),
    Parent(T),
    Unreadable,
}

fn modal_ancestry<T>(
    mut current: T,
    mut read: impl FnMut(T) -> Result<ModalStep<T>, DriverError>,
) -> Result<bool, DriverError> {
    for _ in 0..8 {
        match read(current) {
            Ok(ModalStep::Root(modal)) => return Ok(modal),
            Ok(ModalStep::Parent(parent)) => current = parent,
            Ok(ModalStep::Unreadable) => return Ok(true),
            Err(failure) if failure.kind == ErrorKind::NeedsForeground => return Ok(true),
            Err(failure) => return Err(failure),
        }
    }
    Ok(true)
}

fn in_modal_sheet(element: &AxElement) -> Result<bool, DriverError> {
    modal_ancestry(element.clone(), |element| {
        let role = element.modal_attribute_string("AXRole")?;
        if role.is_none() {
            return Ok(ModalStep::Unreadable);
        }
        if role.as_deref() == Some("AXSheet") {
            return Ok(ModalStep::Root(true));
        }
        if matches!(role.as_deref(), Some("AXWindow" | "AXApplication")) {
            return Ok(ModalStep::Root(
                element.modal_attribute_bool("AXModal")?.unwrap_or(false),
            ));
        }
        Ok(element
            .modal_attribute_element("AXParent")?
            .map_or(ModalStep::Unreadable, ModalStep::Parent))
    })
}

fn check_recipient_security(
    approved: bool,
    target_secure: bool,
    app_secure: bool,
    system_secure: bool,
) -> Result<(), DriverError> {
    if !approved && (target_secure || app_secure || system_secure) {
        return Err(error(
            ErrorKind::NeedsSecureApproval,
            "Entering text into a password field requires approval",
        ));
    }
    Ok(())
}

fn check_background_keyboard(
    pid: i32,
    application: &AxElement,
    target: &AxElement,
) -> Result<(), DriverError> {
    let focused = application
        .attribute_element("AXFocusedUIElement")?
        .ok_or_else(|| foreground("The app has no focused element"))?;
    let target_pid = target.pid()?;
    let focused_pid = focused.pid()?;
    // Reject foreign handles before asking their ancestry any more questions.
    check_keyboard_recipient(pid, target_pid, focused_pid, false)?;
    check_keyboard_recipient(pid, target_pid, focused_pid, in_modal_sheet(&focused)?)
}

impl MacDriver {
    pub(super) fn act(&mut self, params: ActParams) -> Result<ActOutcome, DriverError> {
        capture::check_unlocked()?;
        let app = apps::resolve_approved(&params.app)?;
        let pid = app
            .pid
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        let application = AxElement::application(pid)?;
        let windows = capture::windows()?;
        let (ax_window, window) = resolve_window(&application, pid, &windows, params.window)
            .map_err(|failure| {
                if failure.kind == ErrorKind::WindowNotFound {
                    stale()
                } else {
                    failure
                }
            })?;
        let id = window.id;
        let table = self.element_tables.get(&(pid, id)).ok_or_else(stale)?;
        let frame = ax_window.position_size()?;
        if !frame
            .into_iter()
            .zip(table.window_frame)
            .all(|(a, b)| a.is_finite() && (a - b).abs() <= 1.0)
        {
            return Err(stale());
        }
        let mut outcome = ActOutcome {
            foreground_used: false,
            note: None,
            scroll_position: None,
            observation: None,
        };
        let background = (|| {
            match &params.action {
                Action::Click {
                    target,
                    button,
                    count,
                    modifiers,
                } => {
                    // A point target only needs the window stale check above.
                    // Unsupported variants must not reach the native hit test.
                    let needs_foreground = *count == 2 || !modifiers.is_empty();
                    let click_message = "Double clicks and modified clicks require the foreground";
                    if matches!(target, Target::Point { .. }) {
                        if needs_foreground {
                            return Err(foreground(click_message));
                        }
                        if *button == Button::Right {
                            return Err(foreground("Right point clicks require the foreground"));
                        }
                    }
                    let hidden = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                        .is_some_and(|app| app.isHidden());
                    let element = click_target_element(table, &application, *target, hidden)?;
                    if needs_foreground {
                        return Err(foreground(click_message));
                    }
                    match target {
                        Target::Element { .. } => {
                            let action = if *button == Button::Left {
                                "AXPress"
                            } else {
                                "AXShowMenu"
                            };
                            if !element.actions()?.iter().any(|a| a == action) {
                                return Err(foreground(format!(
                                    "The element does not support {action}"
                                )));
                            }
                            element.perform(action)?;
                        }
                        Target::Point { point } => {
                            let mut candidate = Some(element);
                            // The hit element plus at most five ancestors.
                            let mut pressed = false;
                            for depth in 0..=5 {
                                let Some(element) = candidate else { break };
                                if element.actions()?.iter().any(|a| a == "AXPress") {
                                    element.perform("AXPress")?;
                                    pressed = true;
                                    break;
                                }
                                if depth == 5 {
                                    break;
                                }
                                candidate = element.attribute_element("AXParent")?;
                            }
                            if !pressed {
                                return Err(foreground(format!(
                                    "no pressable element at ({},{})",
                                    point[0], point[1]
                                )));
                            }
                        }
                    }
                }
                Action::Type { text, target } => {
                    let element = focus(table, &application, *target)?;
                    check_type_recipient(&element, text)?;
                    check_background_keyboard(pid, &application, &element)?;
                    check_secure_approval(&application, Some(&element), params.secure_approved)?;
                    if target.is_some() {
                        let focused = application
                            .attribute_element("AXFocusedUIElement")?
                            .ok_or_else(|| foreground("The app has no focused element"))?;
                        focused.collapse_selection_to_end();
                        check_secure_approval(
                            &application,
                            Some(&element),
                            params.secure_approved,
                        )?;
                    }
                    let recipient = application
                        .attribute_element("AXFocusedUIElement")?
                        .ok_or_else(|| foreground("The app has no focused element"))?;
                    check_type_recipient(&recipient, text)?;
                    events::post_unicode_text(pid, text, || {
                        verify_type_recipient(&application, &recipient, false)
                    })?;
                }
                Action::Key { keys, target } => {
                    let stroke =
                        parse_combo(keys).map_err(|message| error(ErrorKind::Failed, message))?;
                    // On the real Mac, process-directed Cmd+A/Cmd+S events
                    // reached the app but did not dispatch its menu shortcut.
                    let element = focus(table, &application, *target)?;
                    check_background_keyboard(pid, &application, &element)?;
                    if stroke.enters_text() {
                        check_secure_approval(
                            &application,
                            Some(&element),
                            params.secure_approved,
                        )?;
                    }
                    if stroke.modifiers.cmd {
                        return Err(foreground("Command shortcuts require the foreground"));
                    }
                    events::post_key(pid, stroke)?;
                }
                Action::Scroll {
                    target,
                    direction,
                    amount,
                } => {
                    let element = target_element(table, &application, *target)?;
                    outcome.scroll_position = scroll(table, element, *direction, *amount)?;
                }
            }
            Ok(())
        })();
        if let Err(mut failure) = background {
            if failure.kind != ErrorKind::NeedsForeground {
                return Err(failure);
            }
            if !params.foreground_fallback {
                failure
                    .message
                    .push_str("; the foreground fallback is turned off");
                return Err(failure);
            }
            outcome.note = Some(foreground::run(
                pid,
                &application,
                &ax_window,
                table,
                &params,
                &self.aborted,
            )?);
            outcome.foreground_used = true;
        }
        if params.observe_after {
            let observation = self.observe(ObserveParams {
                app: params.app,
                window: Some(id),
                query: None,
                max_elements: 300,
                include_screenshot: params.include_screenshot,
            });
            match observation {
                Ok(observation) => outcome.observation = Some(observation),
                Err(_) if window_gone(&application, &ax_window, pid) => {
                    let note = outcome.note.get_or_insert_with(String::new);
                    if !note.is_empty() {
                        note.push_str("; ");
                    }
                    note.push_str("The window closed after the action; observe again.");
                }
                Err(failure)
                    if failure.kind == ErrorKind::WindowNotFound
                        && ax_window.attribute_bool("AXMinimized").ok().flatten() == Some(true) =>
                {
                    let note = outcome.note.get_or_insert_with(String::new);
                    if !note.is_empty() {
                        note.push_str("; ");
                    }
                    note.push_str(
                        "The window was minimized again after the action; observe again.",
                    );
                }
                Err(failure) => return Err(failure),
            }
        }
        Ok(outcome)
    }
}

fn window_gone(application: &AxElement, window: &AxElement, pid: i32) -> bool {
    if NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_none() {
        return true;
    }
    application
        .elements("AXWindows")
        .is_ok_and(|windows| !windows.iter().any(|candidate| candidate.0 == window.0))
}

pub(super) fn check_type_recipient(element: &AxElement, text: &str) -> Result<(), DriverError> {
    let role = element.attribute_string("AXRole")?;
    crate::type_text::validate_recipient(text, role.as_deref())
        .map_err(|message| error(ErrorKind::Failed, message))
}

pub(super) fn verify_type_recipient(
    application: &AxElement,
    recipient: &AxElement,
    foreground: bool,
) -> Result<(), DriverError> {
    let focused = application.attribute_element("AXFocusedUIElement")?;
    if focused.as_ref().is_none_or(|focus| focus.0 != recipient.0) {
        return Err(error(
            ErrorKind::Failed,
            "The keyboard recipient changed; observe again",
        ));
    }
    if foreground {
        let focused = AxElement::system_wide()?.attribute_element("AXFocusedUIElement")?;
        if focused.as_ref().is_none_or(|focus| focus.0 != recipient.0) {
            return Err(error(
                ErrorKind::Failed,
                "The system keyboard recipient changed; observe again",
            ));
        }
    }
    Ok(())
}

/// Check both the requested target and the actual recipient, after focus/click.
pub(super) fn check_secure_approval(
    application: &AxElement,
    target: Option<&AxElement>,
    approved: bool,
) -> Result<(), DriverError> {
    check_secure_recipients(application, target, approved, None)
}

pub(super) fn check_foreground_secure_approval(
    application: &AxElement,
    target: Option<&AxElement>,
    approved: bool,
) -> Result<(), DriverError> {
    if approved {
        return Ok(());
    }
    let system_focus = AxElement::system_wide()?.attribute_element("AXFocusedUIElement")?;
    check_secure_recipients(application, target, approved, system_focus.as_ref())?;
    if system_focus.is_none() {
        return Err(error(
            ErrorKind::Failed,
            "The system has no focused element",
        ));
    }
    Ok(())
}

fn check_secure_recipients(
    application: &AxElement,
    target: Option<&AxElement>,
    approved: bool,
    system_focus: Option<&AxElement>,
) -> Result<(), DriverError> {
    if approved {
        return Ok(());
    }
    let focused = application.attribute_element("AXFocusedUIElement")?;
    check_recipient_security(
        approved,
        target
            .map(AxElement::is_secure)
            .transpose()?
            .unwrap_or(false),
        focused
            .as_ref()
            .map(AxElement::is_secure)
            .transpose()?
            .unwrap_or(false),
        system_focus
            .map(AxElement::is_secure)
            .transpose()?
            .unwrap_or(false),
    )?;
    if focused.is_none() {
        return Err(error(ErrorKind::Failed, "The app has no focused element"));
    }
    Ok(())
}

fn click_target_element(
    table: &ElementTable,
    application: &AxElement,
    target: Target,
    hidden: bool,
) -> Result<AxElement, DriverError> {
    if hidden && matches!(target, Target::Point { .. }) {
        return Err(foreground("The app is hidden"));
    }
    target_element(table, application, target)
}

pub(super) fn target_element(
    table: &ElementTable,
    application: &AxElement,
    target: Target,
) -> Result<AxElement, DriverError> {
    match target {
        Target::Element { element } => {
            let entry = element
                .checked_sub(1)
                .and_then(|id| table.entries.get(id as usize))
                .ok_or_else(stale)?;
            if entry.element.attribute_string("AXRole")?.as_deref() != Some(entry.role.as_str()) {
                return Err(stale());
            }
            let label = entry.element.label(None)?;
            if label != entry.label {
                return Err(stale());
            }
            entry.element.position_size()?;
            Ok(entry.element.clone())
        }
        Target::Point { point } => {
            let (x, y) = table.geometry.to_screen(point).ok_or_else(|| {
                error(
                    ErrorKind::Failed,
                    "The point is outside the observed window",
                )
            })?;
            application.element_at_position(x as f32, y as f32)
        }
    }
}
fn focus(
    table: &ElementTable,
    application: &AxElement,
    target: Option<Target>,
) -> Result<AxElement, DriverError> {
    if let Some(target) = target {
        let element = target_element(table, application, target)?;
        if element.attribute_string("AXRole")?.as_deref() == Some("AXWindow")
            && application
                .attribute_element("AXFocusedUIElement")?
                .is_none()
        {
            return Err(foreground("The app has no focused element"));
        }
        if let Err(err) = element.set_bool("AXFocused", true) {
            if err.kind == ErrorKind::Stale {
                return Err(err);
            }
            return Err(foreground(format!(
                "Could not focus the target: {}",
                err.message
            )));
        }
        // A successful AX setter on a window can leave no keyboard recipient.
        // Establish this before sending any text/key event, so fallback cannot
        // repeat keyboard delivery whose effects are merely unconfirmed.
        if application
            .attribute_element("AXFocusedUIElement")?
            .is_none()
        {
            return Err(foreground("The app has no focused element"));
        }
        Ok(element)
    } else {
        application
            .attribute_element("AXFocusedUIElement")?
            .ok_or_else(|| foreground("The app has no focused element"))
    }
}

fn scroll(
    table: &ElementTable,
    mut element: AxElement,
    direction: Direction,
    amount: u32,
) -> Result<Option<f64>, DriverError> {
    loop {
        if element.attribute_string("AXRole")?.as_deref() == Some("AXScrollArea") {
            break;
        }
        element = element
            .attribute_element("AXParent")?
            .ok_or_else(|| foreground("No scroll area contains the target"))?;
    }
    let vertical = matches!(direction, Direction::Up | Direction::Down);
    let sign = if matches!(direction, Direction::Up | Direction::Left) {
        -1.0
    } else {
        1.0
    };
    if let Some(bar) = element.attribute_element(if vertical {
        "AXVerticalScrollBar"
    } else {
        "AXHorizontalScrollBar"
    })? {
        let value = bar
            .attribute_string("AXValue")?
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or_else(|| error(ErrorKind::Failed, "The scroll bar has no numeric value"))?;
        let area = element.position_size()?;
        let contents = element.elements("AXContents")?;
        let ratio = contents
            .first()
            .map(|c| c.position_size())
            .transpose()?
            .map(|c| area[3] / c[3])
            .filter(|r| r.is_finite() && *r > 0.0 && *r < 1.0);
        let step = ratio.map_or(0.03, |r| 0.1 * r / (1.0 - r));
        let position = (value + sign * f64::from(amount) * step).clamp(0.0, 1.0);
        bar.set_number("AXValue", position)?;
        return Ok(Some(position));
    }
    let area = element.position_size()?;
    let axis = if vertical { 1 } else { 0 };
    let mut nearest: Option<(f64, AxElement)> = None;
    for entry in &table.entries {
        let frame = entry.element.position_size()?;
        let distance = if sign > 0.0 {
            frame[axis] - area[axis] - area[axis + 2]
        } else {
            area[axis] - frame[axis] - frame[axis + 2]
        };
        if distance < 0.0 || nearest.as_ref().is_some_and(|(d, _)| *d <= distance) {
            continue;
        }
        let mut parent = entry.element.attribute_element("AXParent")?;
        while let Some(node) = parent {
            if node.0 == element.0 {
                if entry
                    .element
                    .actions()?
                    .iter()
                    .any(|a| a == "AXScrollToVisible")
                {
                    nearest = Some((distance, entry.element.clone()));
                }
                break;
            }
            parent = node.attribute_element("AXParent")?;
        }
    }
    let (_, target) = nearest.ok_or_else(|| {
        foreground("No scroll bar or offscreen element supports background scrolling")
    })?;
    target.perform("AXScrollToVisible")?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::CaptureGeometry;

    #[test]
    fn part4_modal_stale_is_not_a_foreground_fallback() {
        let failure = stale();
        assert_eq!(
            modal_ancestry(0, |_| Err(failure.clone())).unwrap_err(),
            failure
        );
    }

    #[test]
    fn part4_modal_timeout_is_not_a_foreground_fallback() {
        let failure = error(ErrorKind::Failed, "Accessibility AXParent timed out");
        assert_eq!(
            modal_ancestry(0, |_| Err(failure.clone())).unwrap_err(),
            failure
        );
    }

    #[test]
    fn modal_uncertainty_requires_foreground() {
        assert!(modal_ancestry(0, |_| Ok(ModalStep::Unreadable)).unwrap());
    }

    #[test]
    fn modal_read_failure_requires_foreground() {
        assert!(
            modal_ancestry(0, |_| Err(error(
                ErrorKind::NeedsForeground,
                "AXModal Failure"
            )))
            .unwrap()
        );
    }

    #[test]
    fn modal_deep_ancestry_requires_foreground() {
        assert!(modal_ancestry(0, |depth| Ok(ModalStep::Parent(depth + 1))).unwrap());
        assert!(
            modal_ancestry(0, |depth| Ok(if depth == 8 {
                ModalStep::Root(false)
            } else {
                ModalStep::Parent(depth + 1)
            }))
            .unwrap()
        );
        assert!(
            !modal_ancestry(0, |depth| Ok(if depth == 7 {
                ModalStep::Root(false)
            } else {
                ModalStep::Parent(depth + 1)
            }))
            .unwrap()
        );
        assert!(modal_ancestry(0, |_| Ok(ModalStep::Root(true))).unwrap());
        assert!(!modal_ancestry(0, |_| Ok(ModalStep::Root(false))).unwrap());
    }

    #[test]
    fn foreground_security_checks_both_app_and_system_recipients() {
        for (target, app, system) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert_eq!(
                check_recipient_security(false, target, app, system)
                    .unwrap_err()
                    .kind,
                ErrorKind::NeedsSecureApproval
            );
            assert!(check_recipient_security(true, target, app, system).is_ok());
        }
        assert!(check_recipient_security(false, false, false, false).is_ok());
    }

    #[test]
    fn foreign_or_modal_keyboard_recipients_require_foreground_before_delivery() {
        assert!(check_keyboard_recipient(10, 10, 10, false).is_ok());
        for (target, focused, modal) in [
            (20, 10, false),
            (10, 20, false),
            (20, 20, false),
            (10, 10, true),
        ] {
            assert_eq!(
                check_keyboard_recipient(10, target, focused, modal)
                    .unwrap_err()
                    .kind,
                ErrorKind::NeedsForeground
            );
        }
    }

    #[test]
    fn hidden_point_clicks_do_not_attempt_native_hit_testing() {
        // A real AX application handle for a nonexistent PID cannot be hit
        // tested. Hidden point clicks must return fallback before that call.
        let application = AxElement::application(i32::MAX).unwrap();
        let table = ElementTable {
            pid: i32::MAX,
            window_id: 1,
            window_frame: [0.0, 0.0, 200.0, 200.0],
            geometry: CaptureGeometry {
                window_origin: (0.0, 0.0),
                window_size: (200.0, 200.0),
                image_size: (200, 200),
            },
            entries: vec![],
        };
        let control = click_target_element(
            &table,
            &application,
            Target::Point {
                point: [100.0, 100.0],
            },
            false,
        )
        .err()
        .expect("native hit test must reject the nonexistent process");
        assert!(matches!(control.kind, ErrorKind::Failed | ErrorKind::Stale));
        let failure = click_target_element(
            &table,
            &application,
            Target::Point {
                point: [100.0, 100.0],
            },
            true,
        )
        .err()
        .expect("hidden point click must need fallback");
        assert_eq!(failure.kind, ErrorKind::NeedsForeground);
        assert_eq!(failure.message, "The app is hidden");
    }
}
