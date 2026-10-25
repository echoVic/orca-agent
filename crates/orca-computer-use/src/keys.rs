#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub cmd: bool,
    pub shift: bool,
    pub option: bool,
    pub ctrl: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyStroke {
    pub keycode: u16,
    pub modifiers: Modifiers,
}

impl KeyStroke {
    /// Keys that can insert text, including paste. Navigation and shortcuts
    /// other than Command+V do not require separate password approval.
    pub fn enters_text(self) -> bool {
        if self.modifiers.ctrl {
            return false;
        }
        if self.modifiers.cmd {
            return self.keycode == 9;
        }
        matches!(self.keycode, 0..=50) && !matches!(self.keycode, 36 | 48)
    }

    /// A shortcut that can go to an app with no window: Command or Control
    /// combinations that cannot enter text, or Escape.
    pub fn is_app_shortcut(self) -> bool {
        !self.enters_text() && (self.modifiers.cmd || self.modifiers.ctrl || self.keycode == 53)
    }

    /// These exact shortcuts act on every window, regardless of the selected one.
    pub fn acts_on_every_window(self) -> bool {
        self.modifiers.cmd
            && !self.modifiers.shift
            && !self.modifiers.ctrl
            && match self.keycode {
                12 => true,                       // Cmd+Q/Cmd+Option+Q
                13 | 46 => self.modifiers.option, // Cmd+Option+W/M
                4 => !self.modifiers.option,      // Cmd+H
                _ => false,
            }
    }

    /// Cmd+N alone: the app-level shortcut that opens a new window, the only
    /// key Orca sends to an app whose windows are all the user's.
    pub fn opens_new_window(self) -> bool {
        self.keycode == 45
            && self.modifiers
                == Modifiers {
                    cmd: true,
                    ..Modifiers::default()
                }
    }
}

fn modifier(name: &str) -> Option<Modifiers> {
    let mut modifiers = Modifiers::default();
    match name {
        "cmd" => modifiers.cmd = true,
        "shift" => modifiers.shift = true,
        "option" | "alt" => modifiers.option = true,
        "ctrl" => modifiers.ctrl = true,
        _ => return None,
    }
    Some(modifiers)
}

/// macOS ANSI virtual keycodes, with Shift for uppercase letters and shifted symbols.
pub fn parse_key(name: &str) -> Result<KeyStroke, String> {
    let (plain, shift) = match name {
        "A" => ("a", true),
        "B" => ("b", true),
        "C" => ("c", true),
        "D" => ("d", true),
        "E" => ("e", true),
        "F" => ("f", true),
        "G" => ("g", true),
        "H" => ("h", true),
        "I" => ("i", true),
        "J" => ("j", true),
        "K" => ("k", true),
        "L" => ("l", true),
        "M" => ("m", true),
        "N" => ("n", true),
        "O" => ("o", true),
        "P" => ("p", true),
        "Q" => ("q", true),
        "R" => ("r", true),
        "S" => ("s", true),
        "T" => ("t", true),
        "U" => ("u", true),
        "V" => ("v", true),
        "W" => ("w", true),
        "X" => ("x", true),
        "Y" => ("y", true),
        "Z" => ("z", true),
        "!" => ("1", true),
        "@" => ("2", true),
        "#" => ("3", true),
        "$" => ("4", true),
        "%" => ("5", true),
        "^" => ("6", true),
        "&" => ("7", true),
        "*" => ("8", true),
        "(" => ("9", true),
        ")" => ("0", true),
        "_" => ("-", true),
        "+" => ("=", true),
        "{" => ("[", true),
        "}" => ("]", true),
        ":" => (";", true),
        "\"" => ("'", true),
        "|" => ("\\", true),
        "<" => (",", true),
        ">" => (".", true),
        "?" => ("/", true),
        "~" => ("`", true),
        _ => (name, false),
    };
    let keycode = match plain {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" => 49,
        "`" => 50,
        "delete" => 51,
        "forward_delete" => 117,
        "escape" => 53,
        "cmd" => 55,
        "shift" => 56,
        "option" | "alt" => 58,
        "ctrl" => 59,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f3" => 99,
        "f8" => 100,
        "f9" => 101,
        "f11" => 103,
        "f10" => 109,
        "f12" => 111,
        "home" => 115,
        "page_up" => 116,
        "end" => 119,
        "f2" => 120,
        "page_down" => 121,
        "f1" => 122,
        "f4" => 118,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return Err(format!("unknown key {name:?}")),
    };
    let mut modifiers = modifier(name).unwrap_or_default();
    modifiers.shift |= shift;
    Ok(KeyStroke { keycode, modifiers })
}

/// Splits "shift+right" or "cmd++" into separate key names; "+" alone is the plus key.
fn split_combos(keys: &[String]) -> Vec<String> {
    keys.iter()
        .flat_map(|key| {
            if key == "+" || !key.contains('+') {
                return vec![key.clone()];
            }
            let (body, plus) = match key.strip_suffix("++") {
                Some(body) => (body, true),
                None => (key.as_str(), false),
            };
            let mut parts: Vec<String> = body.split('+').map(str::to_owned).collect();
            if parts.iter().any(String::is_empty) {
                return vec![key.clone()];
            }
            if plus {
                parts.push("+".into());
            }
            parts
        })
        .collect()
}

pub fn parse_combo(keys: &[String]) -> Result<KeyStroke, String> {
    let keys = split_combos(keys);
    let (main, prefixes) = keys
        .split_last()
        .ok_or_else(|| "key needs non-empty \"keys\"".to_owned())?;
    let mut stroke = parse_key(main)?;
    if modifier(main).is_some() {
        return Err("\"keys\" must end with a main key, not a modifier".into());
    }
    for name in prefixes {
        parse_key(name)?;
        let flags = modifier(name)
            .ok_or_else(|| {
                format!(
                    r#"{name:?} must be a modifier before the main key; "keys" is one shortcut: modifiers first, then one main key, such as ["shift", "right"]. Send it again to press it again."#
                )
            })?;
        stroke.modifiers.cmd |= flags.cmd;
        stroke.modifiers.shift |= flags.shift;
        stroke.modifiers.option |= flags.option;
        stroke.modifiers.ctrl |= flags.ctrl;
    }
    Ok(stroke)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_wide_shortcuts_have_exact_keys_and_modifiers() {
        for key in ["q", "w", "m", "h", "n", "a"] {
            for mask in 0..16 {
                let mut stroke = parse_key(key).unwrap();
                stroke.modifiers = Modifiers {
                    cmd: mask & 1 != 0,
                    option: mask & 2 != 0,
                    shift: mask & 4 != 0,
                    ctrl: mask & 8 != 0,
                };
                assert_eq!(
                    stroke.acts_on_every_window(),
                    (key == "q" && matches!(mask, 1 | 3))
                        || (matches!(key, "w" | "m") && mask == 3)
                        || (key == "h" && mask == 1),
                    "{key} {mask}"
                );
            }
        }
    }

    #[test]
    fn only_cmd_n_alone_opens_a_new_window() {
        assert!(combo(&["cmd", "n"]).opens_new_window());
        for keys in [
            &["cmd", "shift", "n"][..],
            &["cmd", "option", "n"],
            &["ctrl", "n"],
            &["cmd", "w"],
            &["n"],
        ] {
            assert!(!combo(keys).opens_new_window(), "{keys:?}");
        }
    }

    fn combo(keys: &[&str]) -> KeyStroke {
        parse_combo(&keys.iter().map(|k| k.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn plus_joined_combos_are_split_and_a_lone_plus_stays_a_key() {
        let parse =
            |keys: &[&str]| parse_combo(&keys.iter().map(|k| k.to_string()).collect::<Vec<_>>());
        assert_eq!(
            parse(&["shift+right"]).unwrap(),
            parse(&["shift", "right"]).unwrap()
        );
        assert_eq!(
            parse(&["cmd+shift+g"]).unwrap(),
            parse(&["cmd", "shift", "g"]).unwrap()
        );
        assert_eq!(parse(&["cmd++"]).unwrap(), parse(&["cmd", "+"]).unwrap());
        assert_eq!(parse(&["+"]).unwrap(), parse_key("+").unwrap());
        assert!(parse(&["++"]).is_err());
    }

    #[test]
    fn a_second_main_key_explains_the_shortcut_shape() {
        let error = parse_combo(&["shift".into(), "right".into(), "right".into()]).unwrap_err();
        assert_eq!(
            error,
            r#""right" must be a modifier before the main key; "keys" is one shortcut: modifiers first, then one main key, such as ["shift", "right"]. Send it again to press it again."#
        );
    }

    #[test]
    fn shortcut_without_a_window_is_only_for_windowless_apps() {
        assert!(combo(&["cmd", "n"]).is_app_shortcut());
        assert!(combo(&["ctrl", "a"]).is_app_shortcut());
        assert!(combo(&["escape"]).is_app_shortcut());
        // Without an observed focus, nothing that can enter text may be sent.
        assert!(!combo(&["cmd", "v"]).is_app_shortcut());
        assert!(!combo(&["a"]).is_app_shortcut());
        assert!(!combo(&["shift", "right"]).is_app_shortcut());
        assert!(!combo(&["return"]).is_app_shortcut());
    }

    #[test]
    fn cmd_s_is_keycode_1_with_command() {
        let stroke = parse_combo(&["cmd".into(), "s".into()]).unwrap();
        assert_eq!(stroke.keycode, 1);
        assert!(stroke.modifiers.cmd && !stroke.modifiers.shift);
    }

    #[test]
    fn ansi_letters_digits_and_named_keys_have_virtual_keycodes() {
        for (name, code) in [
            ("a", 0),
            ("s", 1),
            ("d", 2),
            ("f", 3),
            ("h", 4),
            ("g", 5),
            ("z", 6),
            ("x", 7),
            ("c", 8),
            ("v", 9),
            ("b", 11),
            ("q", 12),
            ("w", 13),
            ("e", 14),
            ("r", 15),
            ("y", 16),
            ("t", 17),
            ("1", 18),
            ("2", 19),
            ("3", 20),
            ("4", 21),
            ("6", 22),
            ("5", 23),
            ("9", 25),
            ("7", 26),
            ("8", 28),
            ("0", 29),
            ("o", 31),
            ("u", 32),
            ("i", 34),
            ("p", 35),
            ("l", 37),
            ("j", 38),
            ("k", 40),
            ("n", 45),
            ("m", 46),
            ("return", 36),
            ("tab", 48),
            ("space", 49),
            ("delete", 51),
            ("escape", 53),
            ("left", 123),
            ("right", 124),
            ("down", 125),
            ("up", 126),
            ("home", 115),
            ("end", 119),
            ("page_up", 116),
            ("page_down", 121),
            ("f1", 122),
            ("f2", 120),
            ("f3", 99),
            ("f4", 118),
            ("f5", 96),
            ("f6", 97),
            ("f7", 98),
            ("f8", 100),
            ("f9", 101),
            ("f10", 109),
            ("f11", 103),
            ("f12", 111),
        ] {
            let stroke = parse_key(name).unwrap();
            assert_eq!(stroke.keycode, code, "{name}");
            assert_eq!(stroke.modifiers, Modifiers::default(), "{name}");
        }
    }

    #[test]
    fn symbols_and_uppercase_preserve_the_required_shift() {
        for (plain, shifted, code) in [
            ("=", "+", 24),
            ("-", "_", 27),
            ("]", "}", 30),
            ("[", "{", 33),
            ("'", "\"", 39),
            (";", ":", 41),
            ("\\", "|", 42),
            (",", "<", 43),
            ("/", "?", 44),
            (".", ">", 47),
            ("`", "~", 50),
            ("1", "!", 18),
            ("2", "@", 19),
            ("3", "#", 20),
            ("4", "$", 21),
            ("5", "%", 23),
            ("6", "^", 22),
            ("7", "&", 26),
            ("8", "*", 28),
            ("9", "(", 25),
            ("0", ")", 29),
            ("a", "A", 0),
        ] {
            assert_eq!(parse_key(plain).unwrap().keycode, code);
            let stroke = parse_combo(&["cmd".into(), shifted.into()]).unwrap();
            assert_eq!(stroke.keycode, code);
            assert!(stroke.modifiers.cmd && stroke.modifiers.shift, "{shifted}");
        }
    }

    #[test]
    fn modifier_names_and_aliases_are_recognized() {
        for (name, code) in [
            ("cmd", 55),
            ("shift", 56),
            ("option", 58),
            ("alt", 58),
            ("ctrl", 59),
        ] {
            assert_eq!(parse_key(name).unwrap().keycode, code);
        }
        let stroke = parse_combo(&[
            "cmd".into(),
            "shift".into(),
            "alt".into(),
            "ctrl".into(),
            "return".into(),
        ])
        .unwrap();
        assert_eq!(
            stroke.modifiers,
            Modifiers {
                cmd: true,
                shift: true,
                option: true,
                ctrl: true
            }
        );
    }

    #[test]
    fn combos_need_a_main_key_and_only_modifier_prefixes() {
        for keys in [
            vec![],
            vec!["cmd".into()],
            vec!["s".into(), "a".into()],
            vec!["bad".into(), "s".into()],
            vec!["f13".into()],
        ] {
            assert!(parse_combo(&keys).is_err(), "{keys:?}");
        }
        for key in ["", "command", "unknown", "ab"] {
            assert!(parse_key(key).is_err(), "{key}");
        }
    }
}

#[cfg(test)]
#[test]
fn text_entry_classification_distinguishes_shortcuts_and_navigation() {
    for (keys, expected) in [
        (vec!["x"], true),
        (vec!["9"], true),
        (vec!["?"], true),
        (vec!["space"], true),
        (vec!["shift", "option", "x"], true),
        (vec!["cmd", "v"], true),
        (vec!["cmd", "shift", "v"], true),
        (vec!["ctrl", "x"], false),
        (vec!["ctrl", "cmd", "v"], false),
        (vec!["cmd", "a"], false),
        (vec!["cmd", "s"], false),
        (vec!["tab"], false),
        (vec!["return"], false),
        (vec!["escape"], false),
        (vec!["delete"], false),
        (vec!["left"], false),
        (vec!["right"], false),
        (vec!["up"], false),
        (vec!["down"], false),
        (vec!["home"], false),
        (vec!["end"], false),
        (vec!["page_up"], false),
        (vec!["page_down"], false),
        (vec!["f1"], false),
        (vec!["f12"], false),
    ] {
        let keys: Vec<_> = keys.into_iter().map(str::to_owned).collect();
        assert_eq!(
            parse_combo(&keys).unwrap().enters_text(),
            expected,
            "{keys:?}"
        );
    }
}

#[cfg(test)]
#[test]
fn enter_and_forward_delete_are_non_text_keys() {
    for (name, code) in [("enter", 36), ("forward_delete", 117)] {
        let stroke = parse_combo(&[name.into()]).unwrap();
        assert_eq!(stroke.keycode, code);
        assert!(!stroke.enters_text());
    }
}
