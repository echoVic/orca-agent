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
        "return" => 36,
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

pub fn parse_combo(keys: &[String]) -> Result<KeyStroke, String> {
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
            .ok_or_else(|| format!("{name:?} must be a modifier before the main key"))?;
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
        for key in ["", "enter", "command", "unknown", "ab"] {
            assert!(parse_key(key).is_err(), "{key}");
        }
    }
}
