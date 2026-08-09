use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use qwertty::{
    Event as QwerttyEvent, FocusState, Key, KeyEvent as QwerttyKeyEvent,
    KeyEventKind as QwerttyKeyEventKind, Modifiers, MouseButton as QwerttyMouseButton,
    MouseEvent as QwerttyMouseEvent, MouseEventKind as QwerttyMouseEventKind, PasteEvent,
    ScrollDirection,
};

#[derive(Default)]
pub(crate) struct InputAdapter {
    paste: Option<Vec<u8>>,
    legacy_alt_prefix: bool,
}

impl InputAdapter {
    pub(crate) fn adapt(&mut self, event: QwerttyEvent) -> Option<Event> {
        match event {
            QwerttyEvent::Key(key) => {
                let legacy_alt = std::mem::take(&mut self.legacy_alt_prefix);
                let mut key = adapt_key(key)?;
                if legacy_alt {
                    key.modifiers.insert(KeyModifiers::ALT);
                }
                Some(Event::Key(key))
            }
            QwerttyEvent::Mouse(mouse) => {
                self.legacy_alt_prefix = false;
                adapt_mouse(mouse).map(Event::Mouse)
            }
            QwerttyEvent::Focus(focus) => match focus.state() {
                FocusState::Gained => {
                    self.legacy_alt_prefix = false;
                    Some(Event::FocusGained)
                }
                FocusState::Lost => {
                    self.legacy_alt_prefix = false;
                    Some(Event::FocusLost)
                }
                _ => None,
            },
            QwerttyEvent::Resize(resize) => {
                self.legacy_alt_prefix = false;
                let cells = resize.cells();
                Some(Event::Resize(cells.columns(), cells.rows()))
            }
            QwerttyEvent::Paste(paste) => {
                self.legacy_alt_prefix = false;
                self.adapt_paste(paste)
            }
            QwerttyEvent::Syntax(syntax) => self.adapt_syntax(syntax),
            _ => {
                self.legacy_alt_prefix = false;
                None
            }
        }
    }

    fn adapt_syntax(&mut self, syntax: qwertty::SyntaxToken) -> Option<Event> {
        let bytes = syntax.as_bytes();
        if bytes == [0x1b] {
            self.legacy_alt_prefix = true;
            return None;
        }
        self.legacy_alt_prefix = false;
        let [0x1b, byte] = bytes else {
            return None;
        };
        let code = match *byte {
            b'\r' => KeyCode::Enter,
            b'\t' => KeyCode::Tab,
            0x08 | 0x7f => KeyCode::Backspace,
            0x20..=0x7e => KeyCode::Char(char::from(*byte)),
            _ => return None,
        };
        Some(Event::Key(KeyEvent::new(code, KeyModifiers::ALT)))
    }

    fn adapt_paste(&mut self, segment: PasteEvent) -> Option<Event> {
        if segment.is_first() {
            self.paste = Some(Vec::new());
        }
        let bytes = self.paste.as_mut()?;
        bytes.extend_from_slice(segment.data());
        if !segment.is_final() {
            return None;
        }
        let bytes = self.paste.take()?;
        String::from_utf8(bytes).ok().map(Event::Paste)
    }
}

fn adapt_key(key: QwerttyKeyEvent) -> Option<KeyEvent> {
    let mut modifiers = adapt_modifiers(key.modifiers());
    let mut code = match key.key() {
        Key::Char(character) => KeyCode::Char(character),
        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Right => KeyCode::Right,
        Key::Left => KeyCode::Left,
        Key::Enter => KeyCode::Enter,
        Key::Tab if key.modifiers().contains(Modifiers::SHIFT) => KeyCode::BackTab,
        Key::Tab => KeyCode::Tab,
        Key::Backspace => KeyCode::Backspace,
        Key::Escape => KeyCode::Esc,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
        Key::Insert => KeyCode::Insert,
        Key::Delete => KeyCode::Delete,
        Key::Function(number @ 1..=35) => KeyCode::F(number),
        Key::Control(control) => {
            modifiers.insert(KeyModifiers::CONTROL);
            KeyCode::Char(control_character(control)?)
        }
        _ => return None,
    };
    if modifiers.contains(KeyModifiers::SHIFT)
        && let Some(shifted) = key.shifted_key()
    {
        code = KeyCode::Char(shifted);
        modifiers.remove(KeyModifiers::SHIFT);
    }
    let kind = match key.kind() {
        QwerttyKeyEventKind::Press => KeyEventKind::Press,
        QwerttyKeyEventKind::Repeat => KeyEventKind::Repeat,
        QwerttyKeyEventKind::Release => KeyEventKind::Release,
        _ => return None,
    };
    let mut state = KeyEventState::empty();
    if key.modifiers().contains(Modifiers::CAPS_LOCK) {
        state.insert(KeyEventState::CAPS_LOCK);
    }
    if key.modifiers().contains(Modifiers::NUM_LOCK) {
        state.insert(KeyEventState::NUM_LOCK);
    }
    Some(KeyEvent::new_with_kind_and_state(
        code, modifiers, kind, state,
    ))
}

fn control_character(control: u8) -> Option<char> {
    match control {
        0 => Some(' '),
        1..=26 => Some(char::from(b'a' + control - 1)),
        28..=31 => Some(char::from(b'4' + control - 28)),
        _ => None,
    }
}

fn adapt_modifiers(modifiers: Modifiers) -> KeyModifiers {
    let mut adapted = KeyModifiers::empty();
    for (source, target) in [
        (Modifiers::SHIFT, KeyModifiers::SHIFT),
        (Modifiers::CTRL, KeyModifiers::CONTROL),
        (Modifiers::ALT, KeyModifiers::ALT),
        (Modifiers::SUPER, KeyModifiers::SUPER),
        (Modifiers::HYPER, KeyModifiers::HYPER),
        (Modifiers::META, KeyModifiers::META),
    ] {
        if modifiers.contains(source) {
            adapted.insert(target);
        }
    }
    adapted
}

fn adapt_mouse(mouse: QwerttyMouseEvent) -> Option<MouseEvent> {
    let column = mouse.column().checked_sub(1)?;
    let row = mouse.row().checked_sub(1)?;
    let kind = match mouse.kind() {
        QwerttyMouseEventKind::Press => MouseEventKind::Down(adapt_mouse_button(mouse.button())?),
        QwerttyMouseEventKind::Release => MouseEventKind::Up(adapt_mouse_button(mouse.button())?),
        QwerttyMouseEventKind::Moved => match mouse.button() {
            QwerttyMouseButton::None => MouseEventKind::Moved,
            button => MouseEventKind::Drag(adapt_mouse_button(button)?),
        },
        QwerttyMouseEventKind::Scroll(direction) => match direction {
            ScrollDirection::Up => MouseEventKind::ScrollUp,
            ScrollDirection::Down => MouseEventKind::ScrollDown,
            ScrollDirection::Left => MouseEventKind::ScrollLeft,
            ScrollDirection::Right => MouseEventKind::ScrollRight,
            _ => return None,
        },
        _ => return None,
    };
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers: adapt_modifiers(mouse.modifiers()),
    })
}

fn adapt_mouse_button(button: QwerttyMouseButton) -> Option<MouseButton> {
    match button {
        QwerttyMouseButton::Left => Some(MouseButton::Left),
        QwerttyMouseButton::Middle => Some(MouseButton::Middle),
        QwerttyMouseButton::Right => Some(MouseButton::Right),
        QwerttyMouseButton::Other(_) | QwerttyMouseButton::None => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
        MouseEventKind,
    };
    use qwertty::{
        Event as QwerttyEvent, Key, KeyEvent as QwerttyKeyEvent,
        KeyEventKind as QwerttyKeyEventKind, Modifiers, SemanticDecoder,
    };

    use super::InputAdapter;

    fn decode(bytes: &[u8]) -> Vec<QwerttyEvent> {
        let mut decoder = SemanticDecoder::new();
        let mut events = decoder.feed(bytes);
        events.extend(decoder.finish());
        events
    }

    fn adapt_key(key: QwerttyKeyEvent) -> KeyEvent {
        let mut adapter = InputAdapter::default();
        match adapter.adapt(QwerttyEvent::Key(key)) {
            Some(Event::Key(key)) => key,
            other => panic!("expected adapted key, got {other:?}"),
        }
    }

    #[test]
    fn maps_named_character_function_and_control_keys() {
        for (source, expected) in [
            (Key::Char('界'), KeyCode::Char('界')),
            (Key::Up, KeyCode::Up),
            (Key::Down, KeyCode::Down),
            (Key::Left, KeyCode::Left),
            (Key::Right, KeyCode::Right),
            (Key::Enter, KeyCode::Enter),
            (Key::Tab, KeyCode::Tab),
            (Key::Backspace, KeyCode::Backspace),
            (Key::Escape, KeyCode::Esc),
            (Key::Home, KeyCode::Home),
            (Key::End, KeyCode::End),
            (Key::PageUp, KeyCode::PageUp),
            (Key::PageDown, KeyCode::PageDown),
            (Key::Insert, KeyCode::Insert),
            (Key::Delete, KeyCode::Delete),
            (Key::Function(1), KeyCode::F(1)),
            (Key::Function(35), KeyCode::F(35)),
        ] {
            assert_eq!(adapt_key(QwerttyKeyEvent::new(source)).code, expected);
        }

        for (control, expected) in [(0, ' '), (1, 'a'), (26, 'z'), (28, '4'), (31, '7')] {
            let key = adapt_key(QwerttyKeyEvent::new(Key::Control(control)));
            assert_eq!(key.code, KeyCode::Char(expected));
            assert_eq!(key.modifiers, KeyModifiers::CONTROL);
        }
    }

    #[test]
    fn maps_shift_tab_modifiers_kind_and_lock_state() {
        let modifiers = Modifiers::SHIFT
            .union(Modifiers::CTRL)
            .union(Modifiers::ALT)
            .union(Modifiers::SUPER)
            .union(Modifiers::HYPER)
            .union(Modifiers::META)
            .union(Modifiers::CAPS_LOCK)
            .union(Modifiers::NUM_LOCK);
        let key = adapt_key(
            QwerttyKeyEvent::new(Key::Tab)
                .with_modifiers(modifiers)
                .with_kind(QwerttyKeyEventKind::Release),
        );

        assert_eq!(key.code, KeyCode::BackTab);
        assert_eq!(
            key.modifiers,
            KeyModifiers::SHIFT
                | KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META
        );
        assert_eq!(key.kind, KeyEventKind::Release);
        assert_eq!(
            key.state,
            KeyEventState::CAPS_LOCK | KeyEventState::NUM_LOCK
        );

        assert_eq!(
            adapt_key(QwerttyKeyEvent::new(Key::Char('x')).with_kind(QwerttyKeyEventKind::Repeat))
                .kind,
            KeyEventKind::Repeat
        );
    }

    #[test]
    fn maps_reported_shifted_character_like_crossterm() {
        let key = adapt_key(
            QwerttyKeyEvent::new(Key::Char('9'))
                .with_shifted_key('(')
                .with_modifiers(Modifiers::SHIFT.union(Modifiers::ALT)),
        );

        assert_eq!(key.code, KeyCode::Char('('));
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn preserves_legacy_alt_enter_and_alt_character_sequences() {
        let mut adapter = InputAdapter::default();
        let alt_enter = decode(b"\x1b\r")
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();
        assert_eq!(
            alt_enter,
            [Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))]
        );

        let alt_character = decode(b"\x1bx")
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();
        assert_eq!(
            alt_character,
            [Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT
            ))]
        );
    }

    #[test]
    fn decodes_kitty_ctrl_slash_sequence() {
        let mut adapter = InputAdapter::default();
        let events = decode(b"\x1b[47;5u")
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            [Event::Key(KeyEvent::new(
                KeyCode::Char('/'),
                KeyModifiers::CONTROL,
            ))]
        );
    }

    #[test]
    fn unknown_syntax_does_not_prime_the_next_key_as_alt() {
        let mut adapter = InputAdapter::default();
        let adapted = decode(b"\x1b]777;late\x07\r")
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();

        assert_eq!(
            adapted,
            [Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))]
        );
    }

    #[test]
    fn unsupported_key_consumes_legacy_alt_prefix() {
        let mut adapter = InputAdapter::default();
        let mut events = decode(b"\x1b\r");
        let enter = events.pop().expect("enter event");
        let prefix = events.pop().expect("legacy alt prefix");

        assert_eq!(adapter.adapt(prefix), None);
        assert_eq!(
            adapter.adapt(QwerttyEvent::Key(QwerttyKeyEvent::new(Key::Function(0)))),
            None
        );
        assert_eq!(
            adapter.adapt(enter),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            )))
        );
    }

    #[test]
    fn maps_mouse_coordinates_buttons_drag_and_scroll() {
        let fixtures = [
            (
                b"\x1b[<0;1;1M".as_slice(),
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
            ),
            (
                b"\x1b[<1;4;5m".as_slice(),
                MouseEventKind::Up(MouseButton::Middle),
                3,
                4,
            ),
            (
                b"\x1b[<34;8;9M".as_slice(),
                MouseEventKind::Drag(MouseButton::Right),
                7,
                8,
            ),
            (b"\x1b[<35;2;3M".as_slice(), MouseEventKind::Moved, 1, 2),
            (b"\x1b[<64;2;3M".as_slice(), MouseEventKind::ScrollUp, 1, 2),
            (
                b"\x1b[<65;2;3M".as_slice(),
                MouseEventKind::ScrollDown,
                1,
                2,
            ),
            (
                b"\x1b[<66;2;3M".as_slice(),
                MouseEventKind::ScrollLeft,
                1,
                2,
            ),
            (
                b"\x1b[<67;2;3M".as_slice(),
                MouseEventKind::ScrollRight,
                1,
                2,
            ),
        ];

        for (bytes, expected_kind, expected_column, expected_row) in fixtures {
            let source = decode(bytes).pop().expect("one qwertty mouse event");
            let mut adapter = InputAdapter::default();
            let Some(Event::Mouse(mouse)) = adapter.adapt(source) else {
                panic!("expected adapted mouse event");
            };
            assert_eq!(mouse.kind, expected_kind);
            assert_eq!(mouse.column, expected_column);
            assert_eq!(mouse.row, expected_row);
        }
    }

    #[test]
    fn rejects_zero_mouse_coordinates_and_extra_buttons() {
        for bytes in [b"\x1b[<0;0;1M".as_slice(), b"\x1b[<128;1;1M".as_slice()] {
            let source = decode(bytes).pop().expect("one qwertty mouse event");
            assert_eq!(InputAdapter::default().adapt(source), None);
        }
    }

    #[test]
    fn maps_focus_and_resize_and_drops_syntax() {
        let mut adapter = InputAdapter::default();
        let events = decode(b"\x1b[I\x1b[O\x1b[48;24;80;0;0t\x1b]777;late\x07");
        let adapted = events
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();

        assert_eq!(
            adapted,
            [Event::FocusGained, Event::FocusLost, Event::Resize(80, 24)]
        );
    }

    #[test]
    fn reassembles_segmented_utf8_paste_and_rejects_invalid_utf8() {
        let mut decoder = SemanticDecoder::with_payload_limit(3);
        let mut adapter = InputAdapter::default();
        let valid = decoder.feed("\x1b[200~a界b\x1b[201~".as_bytes());
        let adapted = valid
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();
        assert_eq!(adapted, [Event::Paste("a界b".to_string())]);

        let invalid = decoder.feed(b"\x1b[200~\xff\x1b[201~");
        assert!(
            invalid
                .into_iter()
                .all(|event| adapter.adapt(event).is_none())
        );

        let recovery = decoder.feed(b"\x1b[200~ok\x1b[201~");
        let recovered = recovery
            .into_iter()
            .filter_map(|event| adapter.adapt(event))
            .collect::<Vec<_>>();
        assert_eq!(recovered, [Event::Paste("ok".to_string())]);
    }
}
