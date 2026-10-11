//! Short foreground actions always restore the application and pointer they borrowed.
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use objc2_app_kit::NSWorkspace;
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

fn wait_frontmost(pid: i32) -> Result<(), DriverError> {
    let deadline = Instant::now() + Duration::from_secs(1);
    while frontmost_pid() != Some(pid) {
        if Instant::now() >= deadline {
            return Err(error(ErrorKind::Failed, "The app did not become frontmost"));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

struct Restore {
    application: AxElement,
    pid: i32,
    pointer: CGPoint,
    restored: bool,
}

impl Restore {
    fn finish(&mut self) -> Result<(), DriverError> {
        // Always attempt both native restores, even if activation fails.
        let app = self
            .application
            .set_bool("AXFrontmost", true)
            .and_then(|()| wait_frontmost(self.pid));
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
        self.restored = app.is_ok() && pointer.is_ok();
        app.and(pointer)
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
) -> Result<CGPoint, DriverError> {
    let (x, y) = match target {
        Target::Point { point } => table.geometry.to_screen(point).ok_or_else(|| {
            error(
                ErrorKind::Failed,
                "The point is outside the observed window",
            )
        })?,
        Target::Element { .. } => {
            let frame = actions::target_element(table, application, target)?.position_size()?;
            (frame[0] + frame[2] / 2.0, frame[1] + frame[3] / 2.0)
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
    let deadline = Instant::now() + Duration::from_secs(3);
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
    let location = target
        .map(|target| point(table, application, target))
        .transpose()?;
    // A failed background focus must not bypass the password approval gate.
    if matches!(params.action, Action::Type { .. }) && !params.secure_approved {
        let element = if let Some(target) = target {
            Some(actions::target_element(table, application, target)?)
        } else {
            application.attribute_element("AXFocusedUIElement")?
        };
        if element
            .map(|element| element.attribute_string("AXSubrole"))
            .transpose()?
            .flatten()
            .as_deref()
            == Some("AXSecureTextField")
        {
            return Err(error(
                ErrorKind::NeedsSecureApproval,
                "Typing into a password field requires approval",
            ));
        }
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
        application: AxElement::application(old.processIdentifier())?,
        pid: old.processIdentifier(),
        pointer: CGEvent::location(Some(&pointer_event)),
        restored: false,
    };
    let result = (|| {
        check_abort(aborted)?;
        application.set_bool("AXFrontmost", true)?;
        window.perform("AXRaise")?;
        wait_frontmost(pid)?;
        // AX activation/raise can precede AppKit's key-window transition.
        // Keep this pause short and check abort again before any HID post.
        thread::sleep(Duration::from_millis(50));
        check_window(window, table)?;
        let mut hid = HidEvents::new(pid, aborted)?;
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
                    hid.text(text)?;
                }
                Action::Key { keys, .. } => {
                    if let Some(point) = location {
                        hid.click(point, Button::Left, 1, Default::default())?;
                    }
                    hid.key(
                        parse_combo(keys).map_err(|message| error(ErrorKind::Failed, message))?,
                    )?;
                }
            }
            Ok(())
        })();
        hid.release();
        result
    })();
    let restored = restore.finish();
    // Restoration failures are material even when the action also failed.
    restored?;
    result?;
    Ok(format!("used the foreground (switched back to {name})"))
}
