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

pub(super) fn should_collapse_type_selection(was_focused: bool) -> bool {
    !was_focused
}

fn type_target_is_still_focused<T: PartialEq>(
    was_focused: bool,
    target: &T,
    recipient: Option<&T>,
) -> bool {
    was_focused && recipient == Some(target)
}

pub(super) fn keyboard_target_is_focused<T: PartialEq>(
    target: Option<&T>,
    read_recipient: impl FnOnce(Option<&T>) -> Result<Option<T>, DriverError>,
) -> Result<bool, DriverError> {
    let Some(target) = target else {
        return Ok(false);
    };
    Ok(read_recipient(Some(target))?.as_ref() == Some(target))
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

fn modal_ancestry<T: Clone + PartialEq>(
    current: T,
    read: impl FnMut(T) -> Result<ModalStep<T>, DriverError>,
    action_deadline: std::time::Instant,
) -> Result<bool, DriverError> {
    modal_ancestry_until(current, read, action_deadline)
}

fn modal_ancestry_until<T: Clone + PartialEq>(
    mut current: T,
    mut read: impl FnMut(T) -> Result<ModalStep<T>, DriverError>,
    deadline: std::time::Instant,
) -> Result<bool, DriverError> {
    let mut visited = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(error(
                ErrorKind::Failed,
                "The modal check timed out; observe again",
            ));
        }
        if visited.contains(&current) {
            return Ok(true);
        }
        visited.push(current.clone());
        match read(current) {
            Ok(ModalStep::Root(modal)) => return Ok(modal),
            Ok(ModalStep::Parent(parent)) => current = parent,
            Ok(ModalStep::Unreadable) => return Ok(true),
            Err(failure) if failure.kind == ErrorKind::NeedsForeground => return Ok(true),
            Err(failure) => return Err(failure),
        }
    }
}

fn modal_for_recipient<T>(
    read_top_level: impl FnOnce() -> Result<Option<T>, DriverError>,
    inspect_top_level: impl FnOnce(T) -> Result<bool, DriverError>,
    read_window: impl FnOnce() -> Result<Option<T>, DriverError>,
    inspect_window: impl FnOnce(T) -> Result<bool, DriverError>,
    fallback: impl FnOnce() -> Result<bool, DriverError>,
) -> Result<bool, DriverError> {
    let result = (|| {
        let top = read_top_level()?;
        let has_top = top.is_some();
        if top.map(inspect_top_level).transpose()? == Some(true) {
            return Ok(true);
        }
        let window = read_window()?;
        if window.map(inspect_window).transpose()? == Some(true) {
            return Ok(true);
        }
        if has_top { Ok(false) } else { fallback() }
    })();
    match result {
        Err(failure) if failure.kind == ErrorKind::NeedsForeground => Ok(true),
        other => other,
    }
}

fn top_level_is_sheet(role: Option<&str>, subrole: Option<&str>) -> bool {
    role.is_none() || role == Some("AXSheet") || subrole == Some("AXSheet")
}

fn inspect_top_level(element: AxElement) -> Result<bool, DriverError> {
    let role = element.modal_attribute_string("AXRole")?;
    if role.as_deref() == Some("AXSheet") {
        return Ok(true);
    }
    let subrole = element.modal_attribute_string("AXSubrole")?;
    Ok(top_level_is_sheet(role.as_deref(), subrole.as_deref()))
}

fn inspect_modal_window(window: AxElement) -> Result<bool, DriverError> {
    Ok(window.modal_attribute_bool("AXModal")?.unwrap_or(false))
}

fn window_metadata_is_modal(
    role: Option<&str>,
    subrole: Option<&str>,
    modal: Option<bool>,
    parent_role: Option<&str>,
) -> bool {
    top_level_is_sheet(role, subrole) || modal == Some(true) || parent_role == Some("AXSheet")
}

fn inspect_fallback_window(window: AxElement) -> Result<bool, DriverError> {
    let role = window.modal_attribute_string("AXRole")?;
    let subrole = window.modal_attribute_string("AXSubrole")?;
    if window_metadata_is_modal(role.as_deref(), subrole.as_deref(), None, None) {
        return Ok(true);
    }
    let modal = window.modal_attribute_bool("AXModal")?;
    if modal == Some(true) {
        return Ok(true);
    }
    let parent_role = if let Some(parent) = window.modal_attribute_element("AXParent")? {
        Some(
            parent
                .modal_attribute_string("AXRole")?
                .ok_or_else(|| foreground("Unreadable window parent role"))?,
        )
    } else {
        None
    };
    Ok(window_metadata_is_modal(
        role.as_deref(),
        subrole.as_deref(),
        modal,
        parent_role.as_deref(),
    ))
}

fn in_modal_sheet(
    element: &AxElement,
    action_deadline: std::time::Instant,
) -> Result<bool, DriverError> {
    modal_for_recipient(
        || element.modal_attribute_element("AXTopLevelUIElement"),
        inspect_top_level,
        || element.modal_attribute_element("AXWindow"),
        inspect_modal_window,
        || modal_parent_walk(element, action_deadline),
    )
}

fn modal_root(
    role: &str,
    inspect_window: impl FnOnce() -> Result<bool, DriverError>,
    read_application_modal: impl FnOnce() -> Result<bool, DriverError>,
) -> Result<Option<bool>, DriverError> {
    match role {
        "AXSheet" => Ok(Some(true)),
        "AXWindow" => inspect_window().map(Some),
        "AXApplication" => read_application_modal().map(Some),
        _ => Ok(None),
    }
}

fn modal_parent_walk(
    element: &AxElement,
    action_deadline: std::time::Instant,
) -> Result<bool, DriverError> {
    modal_ancestry(
        element.clone(),
        |element| {
            let Some(role) = element.modal_attribute_string("AXRole")? else {
                return Ok(ModalStep::Unreadable);
            };
            if let Some(modal) = modal_root(
                &role,
                || inspect_fallback_window(element.clone()),
                || Ok(element.modal_attribute_bool("AXModal")?.unwrap_or(false)),
            )? {
                return Ok(ModalStep::Root(modal));
            }
            Ok(element
                .modal_attribute_element("AXParent")?
                .map_or(ModalStep::Unreadable, ModalStep::Parent))
        },
        action_deadline,
    )
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
    action_deadline: std::time::Instant,
) -> Result<(), DriverError> {
    let focused = keyboard_recipient(application, Some(target))?
        .ok_or_else(|| foreground("The app has no focused element"))?;
    let target_pid = target.pid()?;
    let focused_pid = focused.pid()?;
    // Reject foreign handles before asking their ancestry any more questions.
    check_keyboard_recipient(pid, target_pid, focused_pid, false)?;
    check_keyboard_recipient(
        pid,
        target_pid,
        focused_pid,
        in_modal_sheet(&focused, action_deadline)?,
    )
}

#[cfg(feature = "test-utils")]
static LAST_ACTION_DEADLINE: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);
#[cfg(feature = "test-utils")]
pub fn test_last_action_deadline() -> Option<std::time::Instant> {
    *LAST_ACTION_DEADLINE.lock().unwrap()
}

impl MacDriver {
    pub(super) fn act(&mut self, params: ActParams) -> Result<ActOutcome, DriverError> {
        let action_deadline = std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT;
        #[cfg(feature = "test-utils")]
        {
            *LAST_ACTION_DEADLINE.lock().unwrap() = Some(action_deadline);
        }

        capture::check_unlocked()?;
        let app = apps::resolve_approved(&params.app)?;
        let pid = app
            .pid
            .ok_or_else(|| error(ErrorKind::AppNotFound, "The app is not running"))?;
        let application = AxElement::application(pid)?;
        let windows = capture::windows()?;
        if params.window.is_none() && !super::app_has_window(&application)? {
            let Action::Key { keys, target: None } = &params.action else {
                return Err(error(
                    ErrorKind::WindowNotFound,
                    crate::messages::no_window(&app.name),
                ));
            };
            let stroke = parse_combo(keys).map_err(|message| error(ErrorKind::Failed, message))?;
            if !stroke.is_app_shortcut() {
                return Err(error(
                    ErrorKind::WindowNotFound,
                    crate::messages::no_window(&app.name),
                ));
            }
            if !params.foreground_fallback {
                return Err(error(
                    ErrorKind::NeedsForeground,
                    "A shortcut for an app without a window needs the foreground; the foreground fallback is turned off",
                ));
            }
            let note = foreground::run_app_shortcut(
                &application,
                &app.name,
                stroke,
                &self.aborted,
                action_deadline,
            )?;
            let mut outcome = ActOutcome {
                foreground_used: true,
                note: Some(note),
                scroll_position: None,
                observation: None,
            };
            if params.observe_after {
                // Cmd+N usually opens a window; show it, or say none appeared.
                match self.observe(ObserveParams {
                    app: params.app,
                    window: None,
                    query: None,
                    max_elements: 300,
                    include_screenshot: params.include_screenshot,
                }) {
                    Ok(observation) => outcome.observation = Some(observation),
                    Err(_) => {
                        let note = outcome.note.get_or_insert_with(String::new);
                        note.push_str("; no window appeared yet; observe again");
                    }
                }
            }
            return Ok(outcome);
        }
        let (ax_window, window) =
            resolve_window(&application, pid, &app.name, &windows, params.window)?;
        let id = window.id;
        let table = self
            .element_tables
            .get(&(pid, id))
            .ok_or_else(|| error(ErrorKind::Stale, crate::messages::not_observed(id)))?;
        let frame = ax_window.position_size()?;
        if !frame
            .into_iter()
            .zip(table.window_frame)
            .all(|(a, b)| a.is_finite() && (a - b).abs() <= 1.0)
        {
            return Err(stale());
        }
        // Keep original focus for fallback unless a fresh recipient check
        // proves the target lost it before the background attempt.
        let mut type_target_was_focused = if let Action::Type {
            target: Some(target @ Target::Element { .. }),
            ..
        } = &params.action
        {
            let target = target_element(table, &application, *target)?;
            keyboard_target_is_focused(Some(&target), |target| {
                keyboard_recipient(&application, target)
            })?
        } else {
            false
        };
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
                    // Setting AXFocused again can replace an intentional selection.
                    let element = match target {
                        Some(target) if type_target_was_focused => {
                            let element = target_element(table, &application, *target)?;
                            let recipient = keyboard_recipient(&application, Some(&element))?;
                            type_target_was_focused = type_target_is_still_focused(
                                type_target_was_focused,
                                &element,
                                recipient.as_ref(),
                            );
                            if type_target_was_focused {
                                element
                            } else {
                                focus(table, &application, Some(*target))?
                            }
                        }
                        _ => focus(table, &application, *target)?,
                    };
                    check_type_recipient(&element, text)?;
                    check_background_keyboard(pid, &application, &element, action_deadline)?;
                    check_secure_approval(&application, Some(&element), params.secure_approved)?;
                    if target.is_some() {
                        let focused = keyboard_recipient(&application, Some(&element))?
                            .ok_or_else(|| foreground("The app has no focused element"))?;
                        if should_collapse_type_selection(type_target_was_focused) {
                            focused.collapse_selection_to_end();
                        }
                        check_secure_approval(
                            &application,
                            Some(&element),
                            params.secure_approved,
                        )?;
                    }
                    let recipient = keyboard_recipient(&application, Some(&element))?
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
                    check_background_keyboard(pid, &application, &element, action_deadline)?;
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
                    outcome.scroll_position =
                        scroll(table, element, *direction, *amount, action_deadline)?;
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
                type_target_was_focused,
                &application,
                &ax_window,
                table,
                &params,
                &self.aborted,
                action_deadline,
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
                // The action was delivered: a failure here must not look like
                // one, or the caller could send it again.
                Err(failure) if failure.kind == ErrorKind::WindowNotFound => {
                    let note = outcome.note.get_or_insert_with(String::new);
                    if !note.is_empty() {
                        note.push_str("; ");
                    }
                    note.push_str(crate::messages::WINDOW_UNUSABLE_AFTER_ACTION);
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
    validate_type_with_role(text, || element.attribute_string("AXRole"))
}

fn validate_type_with_role(
    text: &str,
    read_role: impl FnOnce() -> Result<Option<String>, DriverError>,
) -> Result<(), DriverError> {
    crate::type_text::validate_controls(text)
        .map_err(|message| error(ErrorKind::Failed, message))?;
    if !text.contains(['\t', '\n', '\r']) {
        return Ok(());
    }
    let role = read_role()?;
    crate::type_text::validate_recipient(text, role.as_deref())
        .map_err(|message| error(ErrorKind::Failed, message))
}

pub(super) fn verify_type_recipient(
    application: &AxElement,
    recipient: &AxElement,
    foreground: bool,
) -> Result<(), DriverError> {
    let focused = keyboard_recipient(application, Some(recipient))?;
    if focused.as_ref().is_none_or(|focus| focus.0 != recipient.0) {
        return Err(error(
            ErrorKind::Failed,
            "The keyboard recipient changed; observe again",
        ));
    }
    if recipient.is_secure()? {
        return Err(error(
            ErrorKind::Failed,
            "The keyboard recipient became secure; observe again",
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
    let focused = keyboard_recipient(application, target)?;
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
fn focused_or_verified_target<T: Clone>(
    focused: Option<T>,
    target: Option<&T>,
    read: impl FnOnce(&T) -> Result<Option<bool>, DriverError>,
) -> Result<Option<T>, DriverError> {
    if focused.is_some() {
        return Ok(focused);
    }
    if let Some(target) = target
        && read(target)? == Some(true)
    {
        return Ok(Some(target.clone()));
    }
    Ok(None)
}

pub(super) fn keyboard_recipient(
    application: &AxElement,
    target: Option<&AxElement>,
) -> Result<Option<AxElement>, DriverError> {
    focused_or_verified_target(
        application.attribute_element("AXFocusedUIElement")?,
        target,
        |element| element.attribute_bool("AXFocused"),
    )
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
        if keyboard_recipient(application, Some(&element))?.is_none() {
            return Err(foreground("The app has no focused element"));
        }
        Ok(element)
    } else {
        application
            .attribute_element("AXFocusedUIElement")?
            .ok_or_else(|| foreground("The app has no focused element"))
    }
}

// Share the candidate scan with fake accessibility reads in unit tests.
fn nearest_offscreen_scroll_target<T: Clone + PartialEq>(
    entries: impl IntoIterator<Item = T>,
    scroll_area: &T,
    deadline: std::time::Instant,
    mut distance: impl FnMut(&T) -> Result<f64, DriverError>,
    mut read_parent: impl FnMut(&T) -> Result<Option<T>, DriverError>,
    mut supports_scroll: impl FnMut(&T) -> Result<bool, DriverError>,
) -> Result<T, DriverError> {
    let mut nearest: Option<(f64, T)> = None;
    'entries: for entry in entries {
        check_scroll_deadline(deadline)?;
        let Ok(distance) = distance(&entry) else {
            continue;
        };
        check_scroll_deadline(deadline)?;
        if distance < 0.0 || nearest.as_ref().is_some_and(|(d, _)| *d <= distance) {
            continue;
        }
        let Ok(mut parent) = read_parent(&entry) else {
            continue;
        };
        while let Some(node) = parent {
            check_scroll_deadline(deadline)?;
            if &node == scroll_area {
                if supports_scroll(&entry).unwrap_or(false) {
                    nearest = Some((distance, entry.clone()));
                }
                break;
            }
            parent = match read_parent(&node) {
                Ok(parent) => parent,
                Err(_) => continue 'entries,
            };
        }
    }
    check_scroll_deadline(deadline)?;
    nearest.map(|(_, target)| target).ok_or_else(|| {
        foreground("No scroll bar or offscreen element supports background scrolling")
    })
}

fn check_scroll_deadline(deadline: std::time::Instant) -> Result<(), DriverError> {
    if std::time::Instant::now() >= deadline {
        return Err(error(
            ErrorKind::Failed,
            "The background scroll timed out; observe again",
        ));
    }
    Ok(())
}

fn scroll(
    table: &ElementTable,
    mut element: AxElement,
    direction: Direction,
    amount: u32,
    deadline: std::time::Instant,
) -> Result<Option<f64>, DriverError> {
    loop {
        check_scroll_deadline(deadline)?;
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
    let target = nearest_offscreen_scroll_target(
        table.entries.iter().map(|entry| entry.element.clone()),
        &element,
        deadline,
        |entry| {
            let frame = entry.position_size()?;
            Ok(if sign > 0.0 {
                frame[axis] - area[axis] - area[axis + 2]
            } else {
                area[axis] - frame[axis] - frame[axis + 2]
            })
        },
        |entry| entry.attribute_element("AXParent"),
        |entry| Ok(entry.actions()?.iter().any(|a| a == "AXScrollToVisible")),
    )?;
    target.perform("AXScrollToVisible")?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::CaptureGeometry;

    #[test]
    fn typing_into_an_already_focused_element_keeps_its_selection() {
        assert!(!should_collapse_type_selection(true));
    }

    #[test]
    fn typing_into_an_unfocused_element_collapses_its_selection() {
        assert!(should_collapse_type_selection(false));
    }

    #[test]
    fn type_focus_recheck_requires_refocus_and_collapse_after_focus_moves() {
        for recipient in [None, Some(7)] {
            let keep_selection = type_target_is_still_focused(true, &42, recipient.as_ref());
            assert!(
                !keep_selection,
                "initial focus alone cannot skip refocusing"
            );
            assert!(should_collapse_type_selection(keep_selection));
        }
    }

    #[test]
    fn type_focus_recheck_preserves_selection_only_for_continuous_target_focus() {
        assert!(type_target_is_still_focused(true, &42, Some(&42)));
        // Newly acquired focus must still collapse any selection made by Cocoa.
        assert!(!type_target_is_still_focused(false, &42, Some(&42)));
        assert!(!type_target_is_still_focused(false, &42, None));
    }

    #[test]
    fn a_target_focus_flag_preserves_selection_when_app_focus_is_missing() {
        for (flag, expected) in [(Some(true), true), (Some(false), false), (None, false)] {
            let focused = keyboard_target_is_focused(Some(&42), |target| {
                focused_or_verified_target(None, target, |_| Ok(flag))
            })
            .unwrap();
            assert_eq!(focused, expected);
            assert_eq!(should_collapse_type_selection(focused), !expected);
        }
    }

    #[test]
    fn another_app_recipient_prevents_skipping_target_focus_or_click() {
        for (recipient, expected) in [(42, true), (7, false)] {
            assert_eq!(
                keyboard_target_is_focused(Some(&42), |target| {
                    focused_or_verified_target(Some(recipient), target, |_| {
                        panic!("the application recipient must take precedence")
                    })
                })
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn keyboard_focus_shortcuts_require_a_target_and_preserve_read_errors() {
        assert!(!keyboard_target_is_focused::<u32>(None, |_| panic!("no element target")).unwrap());
        let failure = stale();
        assert_eq!(
            keyboard_target_is_focused(Some(&42), |_| Err(failure.clone())).unwrap_err(),
            failure
        );
    }

    fn offscreen_target_with_stale_read(stage: &str) -> Result<u32, DriverError> {
        nearest_offscreen_scroll_target(
            [1, 2],
            &9,
            std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT,
            |entry| {
                if *entry == 1 && stage == "frame" {
                    Err(stale())
                } else {
                    Ok(f64::from(*entry))
                }
            },
            |entry| match (*entry, stage) {
                (1, "parent") | (5, "ancestor") => Err(stale()),
                (1, "ancestor") => Ok(Some(5)),
                _ => Ok(Some(9)),
            },
            |_| Ok(true),
        )
    }

    #[test]
    fn offscreen_scroll_skips_stale_position_and_size() {
        assert_eq!(offscreen_target_with_stale_read("frame").unwrap(), 2);
    }

    #[test]
    fn offscreen_scroll_skips_stale_parent() {
        assert_eq!(offscreen_target_with_stale_read("parent").unwrap(), 2);
    }

    #[test]
    fn offscreen_scroll_skips_stale_ancestor() {
        assert_eq!(offscreen_target_with_stale_read("ancestor").unwrap(), 2);
    }

    #[test]
    fn offscreen_scroll_expired_deadline_prevents_all_reads() {
        let mut reads = 0;
        let result = nearest_offscreen_scroll_target(
            [1],
            &9,
            std::time::Instant::now() - std::time::Duration::from_secs(1),
            |_| {
                reads += 1;
                Ok(1.0)
            },
            |_| Ok(Some(9)),
            |_| Ok(true),
        );
        assert_eq!(
            result
                .expect_err("an expired action must refuse the scan")
                .kind,
            ErrorKind::Failed
        );
        assert_eq!(reads, 0);
    }

    #[test]
    fn offscreen_scroll_parent_walk_uses_the_existing_deadline() {
        let mut action_reads = 0;
        let result = nearest_offscreen_scroll_target(
            [1],
            &9,
            std::time::Instant::now() + std::time::Duration::from_millis(1),
            |_| Ok(1.0),
            |_| {
                std::thread::sleep(std::time::Duration::from_millis(3));
                Ok(Some(9))
            },
            |_| {
                action_reads += 1;
                Ok(true)
            },
        );
        let failure = result.expect_err("parent reads must not refresh the action budget");
        assert_eq!(failure.kind, ErrorKind::Failed);
        assert!(failure.message.contains("timed out"));
        assert_eq!(action_reads, 0);
    }

    // Default test budget only; production must receive the existing action deadline.
    fn modal_ancestry<T: Clone + PartialEq>(
        current: T,
        read: impl FnMut(T) -> Result<ModalStep<T>, DriverError>,
    ) -> Result<bool, DriverError> {
        super::modal_ancestry(
            current,
            read,
            std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT,
        )
    }

    #[test]
    fn background_focus_uses_only_a_verified_target_when_the_app_omits_focus() {
        assert_eq!(
            focused_or_verified_target(None, Some(&42), |_| Ok(Some(true))).unwrap(),
            Some(42)
        );
        for flag in [None, Some(false)] {
            assert_eq!(
                focused_or_verified_target(None, Some(&42), |_| Ok(flag)).unwrap(),
                None
            );
        }
        assert_eq!(
            focused_or_verified_target(Some(7), Some(&42), |_| panic!("the real focus wins"))
                .unwrap(),
            Some(7)
        );
        assert_eq!(
            focused_or_verified_target::<u32>(None, None, |_| panic!("no target")).unwrap(),
            None
        );
        let error = stale();
        assert_eq!(
            focused_or_verified_target(None, Some(&42), |_| Err(error.clone())).unwrap_err(),
            error
        );
    }

    #[test]
    fn part5_parent_fallback_cannot_refresh_an_expired_action_budget() {
        let mut read = false;
        let result = super::modal_ancestry(
            0,
            |_| {
                read = true;
                Ok(ModalStep::Root(false))
            },
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::Failed);
        assert!(!read);
        assert!(
            !super::modal_ancestry(
                0,
                |_| Ok(ModalStep::Root(false)),
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
    }

    #[test]
    fn part5_sheet_top_level_prevents_background_delivery_through_its_attachment_window() {
        // AppKit: field -> AXSplitGroup -> AXSheet -> standard AXWindow.
        let sheet = ("AXSheet", Some("AXSheet"), false);
        let standard_window = ("AXWindow", Some("AXStandardWindow"), false);
        let modal = modal_for_recipient(
            || Ok(Some(sheet)),
            |top| Ok(top_level_is_sheet(Some(top.0), top.1)),
            || Ok(Some(standard_window)),
            |window| Ok(window.2),
            || panic!("both shortcuts are readable"),
        )
        .unwrap();
        assert_eq!(
            check_keyboard_recipient(10, 10, 10, modal)
                .unwrap_err()
                .kind,
            ErrorKind::NeedsForeground
        );
    }

    #[test]
    fn part5_sheet_subrole_on_top_level_also_prevents_background_delivery() {
        let modal = modal_for_recipient(
            || Ok(Some(("AXWindow", Some("AXSheet")))),
            |top| Ok(top_level_is_sheet(Some(top.0), top.1)),
            || Ok(Some(("AXWindow", Some("AXStandardWindow")))),
            |_| Ok(false),
            || panic!("shortcuts are readable"),
        )
        .unwrap();
        assert!(modal);
    }

    #[test]
    fn parent_fallback_distinguishes_sheet_window_and_application_roots() {
        assert_eq!(
            modal_root(
                "AXSheet",
                || panic!("not an attachment window"),
                || panic!("not an application")
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            modal_root("AXWindow", || Ok(true), || panic!("not an application")).unwrap(),
            Some(true)
        );
        assert_eq!(
            modal_root("AXWindow", || Ok(false), || panic!("not an application")).unwrap(),
            Some(false)
        );
        assert_eq!(
            modal_root("AXApplication", || panic!("not a window"), || Ok(false)).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn part6_fallback_window_uses_sheet_subroles_and_sheet_parents() {
        for (role, subrole, modal, parent_role, expected) in [
            (
                "AXWindow",
                Some("AXSheet"),
                Some(false),
                Some("AXApplication"),
                true,
            ),
            (
                "AXWindow",
                Some("AXStandardWindow"),
                Some(false),
                Some("AXSheet"),
                true,
            ),
            ("AXSheet", None, Some(false), Some("AXWindow"), true),
            ("AXWindow", None, Some(true), Some("AXApplication"), true),
            (
                "AXWindow",
                Some("AXStandardWindow"),
                Some(false),
                Some("AXApplication"),
                false,
            ),
        ] {
            let result = modal_ancestry(0, |_| {
                let modal = modal_root(
                    role,
                    || {
                        Ok(window_metadata_is_modal(
                            Some(role),
                            subrole,
                            modal,
                            parent_role,
                        ))
                    },
                    || panic!("not an application root"),
                )?
                .expect("a sheet or window root ends the walk");
                Ok(ModalStep::Root(modal))
            })
            .unwrap();
            assert_eq!(
                result, expected,
                "{role} {subrole:?}, parent {parent_role:?}"
            );
        }
    }

    #[test]
    fn part4_fallback_window_preserves_inspection_errors() {
        for kind in [
            ErrorKind::Stale,
            ErrorKind::Failed,
            ErrorKind::NeedsForeground,
        ] {
            let failure = error(kind, "window inspection failed");
            assert_eq!(
                modal_root("AXWindow", || Err(failure.clone()), || Ok(false)).unwrap_err(),
                failure
            );
        }
    }

    #[test]
    fn part4_window_unreadable_role_requires_foreground() {
        assert!(top_level_is_sheet(None, None));
    }

    #[test]
    fn part4_window_fallback_timeout_stays_failed_without_reading() {
        let failure = modal_ancestry_until(
            0,
            |_| panic!("expired checks cannot read AX"),
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Failed);
    }

    #[test]
    fn part4_window_avoids_ancestry_reads_for_a_deep_nonmodal_field() {
        let mut fallback_read = false;
        let modal = modal_for_recipient(
            || Ok(Some(1)),
            |_| Ok(false),
            || Ok(Some(1)),
            |_| Ok(false),
            || {
                fallback_read = true;
                modal_ancestry(0, |depth| {
                    Ok(if depth == 40 {
                        ModalStep::Root(false)
                    } else {
                        ModalStep::Parent(depth + 1)
                    })
                })
            },
        )
        .unwrap();
        assert!(
            !modal,
            "a known non-modal top-level element stays background regardless of field depth"
        );
        assert!(!fallback_read);
    }

    #[test]
    fn top_level_sheet_roles_and_ordinary_windows_are_distinct() {
        assert!(top_level_is_sheet(Some("AXSheet"), None));
        assert!(top_level_is_sheet(Some("AXWindow"), Some("AXSheet")));
        assert!(!top_level_is_sheet(
            Some("AXWindow"),
            Some("AXStandardWindow")
        ));
    }

    #[test]
    fn part5_sheet_shortcuts_cover_modal_windows_and_absence_without_a_depth_walk() {
        for (top, window, expected) in [
            (Some(false), Some(false), false),
            (Some(false), Some(true), true),
            (None, Some(false), true),
            (None, Some(true), true),
            (Some(false), None, false),
        ] {
            assert_eq!(
                modal_for_recipient(|| Ok(top), Ok, || Ok(window), Ok, || Ok(true)).unwrap(),
                expected
            );
        }
        assert!(
            modal_for_recipient::<bool>(|| Ok(None), Ok, || Ok(None), Ok, || Ok(true)).unwrap()
        );
        for kind in [
            ErrorKind::Stale,
            ErrorKind::Failed,
            ErrorKind::NeedsForeground,
        ] {
            let failure = error(kind, "top-level read failed");
            let result = modal_for_recipient::<bool>(
                || Err(failure.clone()),
                Ok,
                || panic!("top read failed"),
                Ok,
                || panic!("top read failed"),
            );
            if kind == ErrorKind::NeedsForeground {
                assert!(result.unwrap());
            } else {
                assert_eq!(result.unwrap_err(), failure);
            }
        }
    }

    #[test]
    fn part6_missing_top_level_walks_past_a_nonmodal_attachment_window() {
        for fallback_modal in [true, false] {
            let mut walked = false;
            let modal = modal_for_recipient(
                || Ok(None),
                |_| panic!("no top-level element"),
                || Ok(Some(false)),
                Ok,
                || {
                    walked = true;
                    modal_ancestry(0, |depth| {
                        Ok(if depth == 40 {
                            ModalStep::Root(fallback_modal)
                        } else {
                            ModalStep::Parent(depth + 1)
                        })
                    })
                },
            )
            .unwrap();
            assert_eq!(modal, fallback_modal);
            assert!(walked, "an attachment window cannot rule out a sheet");
        }
    }

    #[test]
    fn part4_window_preserves_errors_and_only_falls_back_for_absence() {
        for kind in [ErrorKind::Stale, ErrorKind::Failed] {
            let failure = error(kind, "required window read failed");
            let result = modal_for_recipient::<u32>(
                || Ok(None),
                |_| panic!("no top-level element"),
                || Err(failure.clone()),
                |_| panic!("no window"),
                || Ok(false),
            );
            assert_eq!(result.unwrap_err(), failure);
        }
        assert!(
            modal_for_recipient::<u32>(
                || Ok(None),
                |_| panic!("no top-level element"),
                || Err(error(ErrorKind::NeedsForeground, "AXWindow Failure")),
                |_| panic!("no window"),
                || panic!("Failure must require foreground")
            )
            .unwrap()
        );
        assert!(
            modal_for_recipient::<u32>(
                || Ok(None),
                |_| panic!("no top-level element"),
                || Ok(None),
                |_| panic!("no window"),
                || Ok(true)
            )
            .unwrap()
        );
        assert!(
            !modal_for_recipient::<u32>(
                || Ok(None),
                |_| panic!("no top-level element"),
                || Ok(None),
                |_| panic!("no window"),
                || Ok(false)
            )
            .unwrap()
        );
    }

    #[test]
    fn part4_window_fallback_has_no_eight_level_cutoff() {
        assert!(
            !modal_ancestry(0, |depth| Ok(if depth == 40 {
                ModalStep::Root(false)
            } else {
                ModalStep::Parent(depth + 1)
            }))
            .unwrap()
        );
    }

    #[test]
    fn part4_ordinary_type_does_not_require_a_recipient_role_read() {
        for text in ["", "Hello 🦀", "approved secret"] {
            let mut read = false;
            let result = validate_type_with_role(text, || {
                read = true;
                Err(stale())
            });
            assert!(
                result.is_ok(),
                "ordinary typing must not add a required AXRole read: {result:?}"
            );
            assert!(!read);
        }
    }

    #[test]
    fn part4_layout_controls_still_require_the_multiline_role() {
        for text in ["a\tb", "a\nb", "a\rb", "a\r\nb"] {
            let mut reads = 0;
            assert!(
                validate_type_with_role(text, || {
                    reads += 1;
                    Ok(Some("AXTextArea".into()))
                })
                .is_ok()
            );
            assert_eq!(reads, 1);
            assert!(
                validate_type_with_role(text, || Ok(Some("AXTextField".into())))
                    .unwrap_err()
                    .message
                    .contains("key")
            );
            let failure = stale();
            assert_eq!(
                validate_type_with_role(text, || Err(failure.clone())).unwrap_err(),
                failure
            );
        }
    }

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
    fn modal_fallback_cycles_are_unreadable_but_known_roots_remain_nonmodal() {
        assert!(modal_ancestry(0, |node| Ok(ModalStep::Parent(1 - node))).unwrap());
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

#[cfg(feature = "test-utils")]
pub fn test_native_modality(
    element: objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>,
    remaining: std::time::Duration,
) -> Result<bool, DriverError> {
    in_modal_sheet(
        &AxElement::from_owned(element)?,
        std::time::Instant::now() + remaining,
    )
}

#[cfg(feature = "test-utils")]
pub fn test_native_type_role(
    element: objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>,
    text: &str,
) -> Result<(), DriverError> {
    check_type_recipient(&AxElement::from_owned(element)?, text)
}

#[cfg(feature = "test-utils")]
pub fn test_native_keyboard_recipient(
    pid: i32,
    target: objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>,
) -> Result<(), DriverError> {
    let application = AxElement::application(pid)?;
    check_background_keyboard(
        pid,
        &application,
        &AxElement::from_owned(target)?,
        std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT,
    )
}
