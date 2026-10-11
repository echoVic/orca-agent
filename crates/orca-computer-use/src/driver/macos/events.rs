//! Process-directed keyboard events use private state and no HID marker.
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID};

use super::error;
use crate::{
    keys::KeyStroke,
    protocol::{DriverError, ErrorKind},
};

pub(super) fn post_unicode_text(pid: i32, text: &str) -> Result<(), DriverError> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .ok_or_else(|| error(ErrorKind::Failed, "Could not create keyboard event source"))?;
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut start = 0;
    while start < units.len() {
        let mut end = (start + 20).min(units.len());
        // Keep a surrogate pair together across the 20-unit boundary.
        if end < units.len() && (0xD800..=0xDBFF).contains(&units[end - 1]) {
            end -= 1;
        }
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(Some(&source), 0, down)
                .ok_or_else(|| error(ErrorKind::Failed, "Could not create keyboard event"))?;
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    (end - start) as u64,
                    units[start..end].as_ptr(),
                )
            };
            CGEvent::post_to_pid(pid, Some(&event));
        }
        start = end;
    }
    Ok(())
}

pub(super) fn post_key(pid: i32, stroke: KeyStroke) -> Result<(), DriverError> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .ok_or_else(|| error(ErrorKind::Failed, "Could not create keyboard event source"))?;
    let mut flags = CGEventFlags::empty();
    for (enabled, flag) in [
        (stroke.modifiers.cmd, CGEventFlags::MaskCommand),
        (stroke.modifiers.shift, CGEventFlags::MaskShift),
        (stroke.modifiers.option, CGEventFlags::MaskAlternate),
        (stroke.modifiers.ctrl, CGEventFlags::MaskControl),
    ] {
        if enabled {
            flags |= flag;
        }
    }
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(Some(&source), stroke.keycode, down)
            .ok_or_else(|| error(ErrorKind::Failed, "Could not create keyboard event"))?;
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::post_to_pid(pid, Some(&event));
    }
    Ok(())
}
