//! Private process-directed background events and marked foreground HID delivery.
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

pub(crate) const HID_MARKER: i64 = 0x4F52_4341;

use crate::{
    keys::Modifiers,
    protocol::{Button, Direction},
};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEventField, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};
use std::sync::atomic::AtomicBool;

fn flags(modifiers: Modifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    for (enabled, flag) in [
        (modifiers.cmd, CGEventFlags::MaskCommand),
        (modifiers.shift, CGEventFlags::MaskShift),
        (modifiers.option, CGEventFlags::MaskAlternate),
        (modifiers.ctrl, CGEventFlags::MaskControl),
    ] {
        if enabled {
            flags |= flag;
        }
    }
    flags
}

pub(super) struct HidEvents<'a> {
    source: CFRetained<CGEventSource>,
    pid: i32,
    aborted: &'a AtomicBool,
    // Retain prebuilt releases before posting each down event. Cleanup cannot
    // fail to allocate the corresponding up, even on an aborted action.
    releases: Vec<CFRetained<CGEvent>>,
}

impl<'a> HidEvents<'a> {
    pub(super) fn new(pid: i32, aborted: &'a AtomicBool) -> Result<Self, DriverError> {
        Ok(Self {
            source: CGEventSource::new(CGEventSourceStateID::Private)
                .ok_or_else(|| error(ErrorKind::Failed, "Could not create HID event source"))?,
            pid,
            aborted,
            releases: vec![],
        })
    }

    fn post(&self, event: &CGEvent) -> Result<(), DriverError> {
        Self::mark(event);
        super::foreground::check_abort(self.aborted)?;
        if super::foreground::frontmost_pid() != Some(self.pid) {
            return Err(error(
                ErrorKind::Failed,
                "The target app is no longer frontmost",
            ));
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
        // Give AppKit a chance to consume the event before restoring focus.
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(())
    }

    fn mark(event: &CGEvent) {
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::EventSourceUserData,
            HID_MARKER,
        );
    }

    fn keyboard(
        &self,
        keycode: u16,
        down: bool,
        modifiers: Modifiers,
    ) -> Result<CFRetained<CGEvent>, DriverError> {
        let event = CGEvent::new_keyboard_event(Some(&self.source), keycode, down)
            .ok_or_else(|| error(ErrorKind::Failed, "Could not create HID keyboard event"))?;
        CGEvent::set_flags(Some(&event), flags(modifiers));
        Ok(event)
    }

    fn modifiers(&mut self, modifiers: Modifiers) -> Result<(), DriverError> {
        let mut active = Modifiers::default();
        for (enabled, keycode) in [
            (modifiers.cmd, 55),
            (modifiers.shift, 56),
            (modifiers.option, 58),
            (modifiers.ctrl, 59),
        ] {
            if !enabled {
                continue;
            }
            let released = active;
            match keycode {
                55 => active.cmd = true,
                56 => active.shift = true,
                58 => active.option = true,
                59 => active.ctrl = true,
                _ => unreachable!(),
            }
            let down = self.keyboard(keycode, true, active)?;
            self.releases.push(self.keyboard(keycode, false, released)?);
            self.post(&down)?;
        }
        Ok(())
    }

    pub(super) fn click(
        &mut self,
        point: CGPoint,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    ) -> Result<(), DriverError> {
        self.modifiers(modifiers)?;
        let (button, down, up) = match button {
            Button::Left => (
                CGMouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
            ),
            Button::Right => (
                CGMouseButton::Right,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
            ),
        };
        let mouse = |kind| {
            CGEvent::new_mouse_event(Some(&self.source), kind, point, button)
                .ok_or_else(|| error(ErrorKind::Failed, "Could not create HID mouse event"))
        };
        let moved = mouse(CGEventType::MouseMoved)?;
        CGEvent::set_flags(Some(&moved), flags(modifiers));
        self.post(&moved)?;
        for count in 1..=count {
            let down = mouse(down)?;
            let up = mouse(up)?;
            for event in [&down, &up] {
                CGEvent::set_flags(Some(event), flags(modifiers));
                CGEvent::set_integer_value_field(
                    Some(event),
                    CGEventField::MouseEventClickState,
                    i64::from(count),
                );
            }
            self.releases.push(up.clone());
            self.post(&down)?;
            self.post(&up)?;
            self.releases.pop();
        }
        Ok(())
    }

    pub(super) fn scroll(
        &self,
        point: CGPoint,
        direction: Direction,
        amount: u32,
    ) -> Result<(), DriverError> {
        let amount = i32::try_from(amount)
            .map_err(|_| error(ErrorKind::Failed, "Scroll amount is too large"))?;
        let (vertical, horizontal) = match direction {
            Direction::Up => (amount, 0),
            Direction::Down => (-amount, 0),
            Direction::Left => (0, amount),
            Direction::Right => (0, -amount),
        };
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&self.source),
            CGScrollEventUnit::Line,
            2,
            vertical,
            horizontal,
            0,
        )
        .ok_or_else(|| error(ErrorKind::Failed, "Could not create HID scroll event"))?;
        CGEvent::set_location(Some(&event), point);
        self.post(&event)
    }

    pub(super) fn key(&mut self, stroke: KeyStroke) -> Result<(), DriverError> {
        self.modifiers(stroke.modifiers)?;
        let down = self.keyboard(stroke.keycode, true, stroke.modifiers)?;
        let up = self.keyboard(stroke.keycode, false, stroke.modifiers)?;
        self.releases.push(up.clone());
        self.post(&down)?;
        self.post(&up)?;
        self.releases.pop();
        Ok(())
    }

    pub(super) fn text(&mut self, text: &str) -> Result<(), DriverError> {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut start = 0;
        while start < units.len() {
            let mut end = (start + 20).min(units.len());
            if end < units.len() && (0xD800..=0xDBFF).contains(&units[end - 1]) {
                end -= 1;
            }
            let down = self.keyboard(0, true, Modifiers::default())?;
            let up = self.keyboard(0, false, Modifiers::default())?;
            for event in [&down, &up] {
                unsafe {
                    CGEvent::keyboard_set_unicode_string(
                        Some(event),
                        (end - start) as u64,
                        units[start..end].as_ptr(),
                    )
                };
            }
            self.releases.push(up.clone());
            self.post(&down)?;
            self.post(&up)?;
            self.releases.pop();
            start = end;
        }
        Ok(())
    }

    pub(super) fn release(&mut self) {
        while let Some(event) = self.releases.pop() {
            Self::mark(&event);
            if super::foreground::frontmost_pid() == Some(self.pid) {
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            } else {
                // If the user switched apps mid-action, deliver the release only
                // to our target; never send HID keyboard input to the new app.
                CGEvent::post_to_pid(self.pid, Some(&event));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Drop for HidEvents<'_> {
    fn drop(&mut self) {
        self.release();
    }
}
