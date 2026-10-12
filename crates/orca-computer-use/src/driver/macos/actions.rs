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
                    events::post_unicode_text(pid, text)?;
                }
                Action::Key { keys, target } => {
                    let stroke =
                        parse_combo(keys).map_err(|message| error(ErrorKind::Failed, message))?;
                    // On the real Mac, process-directed Cmd+A/Cmd+S events
                    // reached the app but did not dispatch its menu shortcut.
                    let element = focus(table, &application, *target)?;
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

/// Check both the requested target and the actual recipient, after focus/click.
pub(super) fn check_secure_approval(
    application: &AxElement,
    target: Option<&AxElement>,
    approved: bool,
) -> Result<(), DriverError> {
    if approved {
        return Ok(());
    }
    let focused = application.attribute_element("AXFocusedUIElement")?;
    for element in target.into_iter().chain(focused.as_ref()) {
        if element.is_secure()? {
            return Err(error(
                ErrorKind::NeedsSecureApproval,
                "Entering text into a password field requires approval",
            ));
        }
    }
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
