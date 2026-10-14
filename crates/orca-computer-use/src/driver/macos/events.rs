//! Private process-directed background events and marked foreground HID delivery.
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID};

use super::error;
use crate::{
    keys::KeyStroke,
    protocol::{DriverError, ErrorKind},
};

const HID_EVENT_DELIVERY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

fn unicode_chunk_end(units: &[u16], start: usize, limit: usize) -> usize {
    // AppKit interprets a leading newline/tab as a command and ignores
    // the rest of that event's Unicode string. Isolate control characters.
    let control = |unit: u16| matches!(unit, 9 | 10 | 13);
    if control(units[start]) {
        return start + 1;
    }
    let mut end = (start + limit).min(units.len());
    if let Some(offset) = units[start..end].iter().position(|unit| control(*unit)) {
        end = start + offset;
    }
    if end < units.len() && (0xD800..=0xDBFF).contains(&units[end - 1]) {
        if end == start + 1 {
            end += 1;
        } else {
            end -= 1;
        }
    }
    end
}

/// Both PID-directed and HID delivery use this splitter and recipient backstop.
fn deliver_text(
    text: &str,
    chunk_limit: usize,
    mut post: impl FnMut(&[u16]) -> Result<(), DriverError>,
    mut verify_recipient: impl FnMut() -> Result<(), DriverError>,
) -> Result<(), DriverError> {
    crate::type_text::validate_controls(text).map_err(|m| error(ErrorKind::Failed, m))?;
    let text = text.replace("\r\n", "\n");
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut start = 0;
    let mut typed = 0;
    while start < units.len() {
        let end = unicode_chunk_end(&units, start, chunk_limit);
        let chunk = &units[start..end];
        post(chunk).map_err(|failure| error(ErrorKind::Failed, format!(
            "Typing stopped after {typed} characters (the current chunk may be incomplete): {}", failure.message
        )))?;
        typed += String::from_utf16_lossy(chunk).chars().count();
        if chunk.iter().any(|unit| matches!(unit, 9 | 10 | 13)) {
            verify_recipient().map_err(|failure| {
                error(
                    ErrorKind::Failed,
                    format!(
                        "Typing stopped after {typed} characters: {}",
                        failure.message
                    ),
                )
            })?;
        }
        start = end;
    }
    Ok(())
}

pub(super) fn check_foreground_text_budget(
    text: &str,
    deadline: std::time::Instant,
) -> Result<(), DriverError> {
    // HID delivery posts a down/up pair for each scalar, with the same
    // CRLF normalization and surrogate-pair preservation as deliver_text.
    let characters = u32::try_from(text.replace("\r\n", "\n").chars().count()).unwrap_or(u32::MAX);
    let estimated = HID_EVENT_DELIVERY_DELAY
        .saturating_mul(2)
        .saturating_mul(characters);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if estimated > remaining {
        return Err(error(
            ErrorKind::Failed,
            "The text cannot fit in the remaining foreground action time; split the text into smaller `type` calls.",
        ));
    }
    Ok(())
}

fn deliver_foreground_text(
    text: &str,
    deadline: std::time::Instant,
    post: impl FnMut(&[u16]) -> Result<(), DriverError>,
    verify_recipient: impl FnMut() -> Result<(), DriverError>,
) -> Result<(), DriverError> {
    check_foreground_text_budget(text, deadline)?;
    deliver_text(text, 1, post, verify_recipient)
}

pub(super) fn post_unicode_text(
    pid: i32,
    text: &str,
    verify_recipient: impl FnMut() -> Result<(), DriverError>,
) -> Result<(), DriverError> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .ok_or_else(|| error(ErrorKind::Failed, "Could not create keyboard event source"))?;
    deliver_text(
        text,
        1,
        |units| {
            for down in [true, false] {
                let event = CGEvent::new_keyboard_event(Some(&source), 0, down)
                    .ok_or_else(|| error(ErrorKind::Failed, "Could not create keyboard event"))?;
                unsafe {
                    CGEvent::keyboard_set_unicode_string(
                        Some(&event),
                        units.len() as u64,
                        units.as_ptr(),
                    )
                };
                CGEvent::post_to_pid(pid, Some(&event));
            }
            // Control commands must be consumed before reading focus again.
            if units.iter().any(|unit| matches!(unit, 9 | 10 | 13)) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(())
        },
        verify_recipient,
    )
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

#[cfg(feature = "test-utils")]
static HID_CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
#[cfg(feature = "test-utils")]
static LAST_TEST_PROCESS_HID_POST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Most recent driver HID post in this process, on Instant's monotonic clock.
#[cfg(feature = "test-utils")]
pub fn last_test_process_hid_post() -> Option<std::time::Instant> {
    let elapsed = LAST_TEST_PROCESS_HID_POST.load(std::sync::atomic::Ordering::SeqCst);
    (elapsed != 0).then(|| {
        *HID_CLOCK
            .get()
            .expect("HID clock initialized before recording")
            + std::time::Duration::from_nanos(elapsed)
    })
}

#[cfg(feature = "test-utils")]
static LAST_KEY_DOWN: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
#[cfg(feature = "test-utils")]
static MOUSE_DOWNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "test-utils")]
pub fn last_test_key_down() -> Option<std::time::Instant> {
    *LAST_KEY_DOWN.lock().unwrap()
}
#[cfg(feature = "test-utils")]
pub fn test_mouse_down_count() -> u64 {
    MOUSE_DOWNS.load(std::sync::atomic::Ordering::SeqCst)
}

fn post_hid(event: &CGEvent) {
    #[cfg(feature = "test-utils")]
    {
        match CGEvent::r#type(Some(event)) {
            CGEventType::KeyDown => {
                *LAST_KEY_DOWN.lock().unwrap() = Some(std::time::Instant::now())
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                MOUSE_DOWNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            _ => {}
        }
    }
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
    #[cfg(feature = "test-utils")]
    {
        let clock = HID_CLOCK.get_or_init(std::time::Instant::now);
        let elapsed = clock.elapsed().as_nanos().clamp(1, u64::MAX as u128) as u64;
        LAST_TEST_PROCESS_HID_POST.fetch_max(elapsed, std::sync::atomic::Ordering::SeqCst);
    }
}

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
    deadline: std::time::Instant,
}

impl<'a> HidEvents<'a> {
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn new(pid: i32, aborted: &'a AtomicBool) -> Result<Self, DriverError> {
        Self::new_until(
            pid,
            aborted,
            std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT,
        )
    }
    pub(super) fn new_until(
        pid: i32,
        aborted: &'a AtomicBool,
        deadline: std::time::Instant,
    ) -> Result<Self, DriverError> {
        Ok(Self {
            source: CGEventSource::new(CGEventSourceStateID::Private)
                .ok_or_else(|| error(ErrorKind::Failed, "Could not create HID event source"))?,
            pid,
            aborted,
            releases: vec![],
            deadline,
        })
    }

    fn post(&self, event: &CGEvent) -> Result<(), DriverError> {
        Self::mark(event);
        check_delivery_ready(self.pid, self.aborted, self.deadline)?;
        post_hid(event);
        // Give AppKit a chance to consume the event before restoring focus.
        std::thread::sleep(HID_EVENT_DELIVERY_DELAY);
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
        check_click_point(self.pid, point)?;
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
            // Recheck immediately before the mouse down, after move/modifiers.
            check_click_point(self.pid, point)?;
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
        // Input methods may forward an ordinary key after its HID up arrives.
        // Let that forwarding finish while the recipient is still frontmost.
        std::thread::sleep(
            std::time::Duration::from_millis(100).min(
                self.deadline
                    .saturating_duration_since(std::time::Instant::now()),
            ),
        );
        Ok(())
    }

    pub(super) fn text(
        &mut self,
        text: &str,
        verify_recipient: impl FnMut() -> Result<(), DriverError>,
    ) -> Result<(), DriverError> {
        deliver_foreground_text(
            text,
            self.deadline,
            |units| {
                let down = self.keyboard(0, true, Modifiers::default())?;
                let up = self.keyboard(0, false, Modifiers::default())?;
                for event in [&down, &up] {
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(
                            Some(event),
                            units.len() as u64,
                            units.as_ptr(),
                        )
                    };
                }
                self.releases.push(up.clone());
                self.post(&down)?;
                self.post(&up)?;
                self.releases.pop();
                Ok(())
            },
            verify_recipient,
        )
    }

    pub(super) fn release(&mut self) {
        while let Some(event) = self.releases.pop() {
            Self::mark(&event);
            // Downs entered the system HID stream; only HID ups can clear
            // system-wide state, even if focus changed before cleanup.
            post_hid(&event);
            std::thread::sleep(HID_EVENT_DELIVERY_DELAY);
        }
    }
}

impl Drop for HidEvents<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Exercise the real driver's held-input cleanup without racing a 10 ms click.
#[cfg(feature = "test-utils")]
pub fn with_held_test_input(
    pid: i32,
    point: CGPoint,
    while_held: impl FnOnce(),
) -> Result<(), DriverError> {
    let aborted = AtomicBool::new(false);
    let mut hid = HidEvents::new(pid, &aborted)?;
    hid.modifiers(Modifiers {
        shift: true,
        ..Modifiers::default()
    })?;
    let moved = CGEvent::new_mouse_event(
        Some(&hid.source),
        CGEventType::MouseMoved,
        point,
        CGMouseButton::Left,
    )
    .ok_or_else(|| {
        error(
            ErrorKind::Failed,
            "Could not create held-input test movement",
        )
    })?;
    hid.post(&moved)?;
    let down = CGEvent::new_mouse_event(
        Some(&hid.source),
        CGEventType::LeftMouseDown,
        point,
        CGMouseButton::Left,
    )
    .ok_or_else(|| error(ErrorKind::Failed, "Could not create held-input test down"))?;
    let up = CGEvent::new_mouse_event(
        Some(&hid.source),
        CGEventType::LeftMouseUp,
        point,
        CGMouseButton::Left,
    )
    .ok_or_else(|| error(ErrorKind::Failed, "Could not create held-input test up"))?;
    CGEvent::set_flags(
        Some(&down),
        flags(Modifiers {
            shift: true,
            ..Modifiers::default()
        }),
    );
    hid.releases.push(up);
    hid.post(&down)?;
    while_held();
    hid.release();
    Ok(())
}

#[cfg(feature = "test-utils")]
pub fn test_check_click_point(pid: i32, point: CGPoint) -> Result<(), DriverError> {
    check_click_point(pid, point)
}
fn check_click_point(pid: i32, point: CGPoint) -> Result<(), DriverError> {
    let hit_pid = super::ax::AxElement::system_wide()
        .and_then(|system| system.element_at_position(point.x as f32, point.y as f32))
        .and_then(|element| element.pid());
    if hit_pid.is_ok_and(|owner| owner == pid) {
        Ok(())
    } else {
        Err(error(
            ErrorKind::Failed,
            "Another window covers the target point",
        ))
    }
}

/// Exercise the real HID click guard with a cover kept in front; full fallback
/// activation would raise the fixture over an ordinary Finder window first.
#[cfg(feature = "test-utils")]
pub fn test_click_at_point(pid: i32, point: CGPoint) -> Result<(), DriverError> {
    let aborted = AtomicBool::new(false);
    HidEvents::new(pid, &aborted)?.click(point, Button::Left, 1, Modifiers::default())
}

fn check_delivery_ready(
    pid: i32,
    aborted: &AtomicBool,
    deadline: std::time::Instant,
) -> Result<(), DriverError> {
    if std::time::Instant::now() >= deadline {
        return Err(error(
            ErrorKind::Failed,
            "The foreground action timed out; observe again",
        ));
    }
    super::foreground::check_abort(aborted)?;
    if super::foreground::frontmost_pid() != Some(pid) {
        return Err(error(
            ErrorKind::Failed,
            "The target app is no longer frontmost",
        ));
    }
    Ok(())
}
#[cfg(test)]
#[test]
fn an_expired_foreground_deadline_refuses_input_before_native_delivery() {
    let failure = check_delivery_ready(
        i32::MAX,
        &AtomicBool::new(false),
        std::time::Instant::now() - std::time::Duration::from_secs(1),
    )
    .unwrap_err();
    assert_eq!(
        failure.message,
        "The foreground action timed out; observe again"
    );
}

#[cfg(test)]
mod unicode_chunk_tests {
    use super::*;

    #[test]
    fn oversized_foreground_text_posts_nothing() {
        let mut posts = 0;
        let result = deliver_foreground_text(
            &"🦀".repeat(1600),
            std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT,
            |_| {
                posts += 1;
                Ok(())
            },
            || Ok(()),
        );
        assert_eq!(
            posts, 0,
            "over-budget foreground typing must not post a prefix"
        );
        let failure = result.unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Failed);
        assert!(failure.message.contains("split the text"));
        assert!(failure.message.contains("type"));
    }

    #[test]
    fn foreground_text_uses_the_remaining_budget() {
        let mut posts = 0;
        let result = deliver_foreground_text(
            "ab",
            std::time::Instant::now() + std::time::Duration::from_millis(30),
            |_| {
                posts += 1;
                Ok(())
            },
            || Ok(()),
        );
        assert_eq!(
            posts, 0,
            "40 ms of typing cannot fit in the remaining 30 ms"
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::Failed);
    }

    #[test]
    fn foreground_text_budget_counts_scalars_and_normalized_crlf() {
        for text in ["🦀", "\r\n"] {
            let mut posted = vec![];
            deliver_foreground_text(
                text,
                std::time::Instant::now() + std::time::Duration::from_millis(30),
                |units| {
                    posted.push(String::from_utf16(units).unwrap());
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
            assert_eq!(posted, [if text == "🦀" { "🦀" } else { "\n" }]);
        }
    }

    #[test]
    fn foreground_text_that_fits_is_delivered_completely() {
        let mut posted = String::new();
        deliver_foreground_text(
            "Hello\t🦀!",
            std::time::Instant::now() + crate::timing::FOREGROUND_ACTION_LIMIT,
            |units| {
                posted.push_str(&String::from_utf16(units).unwrap());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(posted, "Hello\t🦀!");
    }

    fn chunks(text: &str) -> Vec<String> {
        let mut result = vec![];
        deliver_text(
            text,
            20,
            |units| {
                result.push(String::from_utf16(units).expect("chunks preserve surrogate pairs"));
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        result
    }

    #[test]
    fn hid_unicode_events_carry_one_scalar_even_with_controls_and_surrogate_pairs() {
        let mut posted = vec![];
        deliver_text(
            "Ab\tC\r\n🦀/Z",
            1,
            |units| {
                posted.push(String::from_utf16(units).unwrap());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(posted, ["A", "b", "\t", "C", "\n", "🦀", "/", "Z"]);
    }

    #[test]
    fn part4_function_key_characters_refuse_all_chunks_before_background_or_hid_delivery() {
        for code in 0xf700..=0xf8ff {
            let text = format!(
                "{}{}suffix",
                "AbC/".repeat(12),
                char::from_u32(code).unwrap()
            );
            let mut posted = false;
            let failure = deliver_text(
                &text,
                20,
                |_| {
                    posted = true;
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap_err();
            assert_eq!(failure.kind, ErrorKind::Failed);
            assert!(failure.message.contains("key"));
            assert!(!posted, "no prefix may be posted for U+{code:04X}");
        }
        assert_eq!(chunks("\u{f6ff}\u{f900}🦀").concat(), "\u{f6ff}\u{f900}🦀");
    }
    #[test]
    fn unicode_chunks_treat_crlf_as_one_newline_for_background_and_hid() {
        assert_eq!(chunks("a\r\nb\t🦀"), ["a", "\n", "b", "\t", "🦀"]);
    }
    #[test]
    fn a_control_chunk_rechecks_focus_and_never_posts_the_remaining_text() {
        let mut posted = String::new();
        let failure = deliver_text(
            "user\tpass",
            20,
            |units| {
                posted.push_str(&String::from_utf16(units).unwrap());
                Ok(())
            },
            || Err(error(ErrorKind::Failed, "recipient changed")),
        )
        .unwrap_err();
        assert_eq!(posted, "user\t");
        assert!(failure.message.contains("5 characters"));
        assert_eq!(
            failure.kind,
            ErrorKind::Failed,
            "a partial type must never replay via fallback"
        );
    }

    #[test]
    fn forbidden_controls_refuse_the_whole_text_before_any_chunk() {
        let mut posted = false;
        assert!(
            deliver_text(
                "prefix\u{7}suffix",
                20,
                |_| {
                    posted = true;
                    Ok(())
                },
                || Ok(())
            )
            .is_err()
        );
        assert!(!posted);
    }

    #[test]
    fn unicode_chunks_isolate_controls_and_keep_surrogate_pairs() {
        assert_eq!(
            chunks("AbC\n/xyZ\tEnd\r🦀"),
            ["AbC", "\n", "/xyZ", "\t", "End", "\r", "🦀"]
        );
        let text = format!("{}🦀Z", "x".repeat(19));
        assert_eq!(chunks(&text), ["x".repeat(19), "🦀Z".into()]);
        assert_eq!(chunks(&text).concat(), text);
    }
}

#[cfg(feature = "test-utils")]
pub fn test_foreground_text(pid: i32, text: &str) -> Result<(), DriverError> {
    let application = super::ax::AxElement::application(pid)?;
    let recipient = application
        .attribute_element("AXFocusedUIElement")?
        .ok_or_else(|| error(ErrorKind::Failed, "No test recipient"))?;
    super::actions::check_type_recipient(&recipient, text)?;
    super::actions::check_foreground_secure_approval(&application, Some(&recipient), false)?;
    let aborted = AtomicBool::new(false);
    let mut hid = HidEvents::new(pid, &aborted)?;
    let result = hid.text(text, || {
        super::actions::verify_type_recipient(&application, &recipient, true)
    });
    hid.release();
    result
}
