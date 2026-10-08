//! The text of a TOML file, as Orca hands it to the parser.
//!
//! A line break inside a multi-line string, `'''...'''` or `"""..."""`, reads
//! as LF whatever the file wrote: toml 0.8 did that, and the value of a
//! string such as a hook's `command` or a description is what it made. toml
//! 1.x keeps what the file wrote, so a file saved with CRLF line ends, as
//! Windows editors do, would give such a string a CR before every LF, and a
//! hook's command would reach the shell with them.
//!
//! So a reader of a TOML file that a user writes passes the text through
//! [`crlf_to_lf`] before it parses it, and gives the parser and
//! [`syntax_error_text`](super::error_text::syntax_error_text) that same
//! text, so that a position in a message is a position in it. Files that
//! Orca writes itself do not need it, and neither do the editors of the user
//! config in `user_edit`, which parse the text as it is to change it in
//! place.

use std::borrow::Cow;

/// `text` with every CRLF an LF. Outside a multi-line string a CRLF is only a
/// line end, so no other value reads differently.
///
/// A CR that no LF follows is invalid TOML wherever it stands, and a text
/// with one is left as it is: the LF of a CRLF right behind it would join it
/// to a line end and make the text valid.
pub fn crlf_to_lf(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let has_a_lone_cr = bytes
        .iter()
        .enumerate()
        .any(|(index, &byte)| byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'));
    if has_a_lone_cr {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text.replace("\r\n", "\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_without_a_cr_is_not_copied() {
        for text in ["", "a = 1\n", "a = '''x\ny'''\n\n"] {
            assert!(matches!(crlf_to_lf(text), Cow::Borrowed(_)), "{text:?}");
        }
    }

    #[test]
    fn every_crlf_becomes_an_lf() {
        for (text, expected) in [
            ("a = 1\r\nb = 2\r\n", "a = 1\nb = 2\n"),
            ("a = 1\r\n\r\n\r\nb = 2", "a = 1\n\n\nb = 2"),
            // Some of each, and a byte order mark.
            ("a\r\nb\nc\r\n", "a\nb\nc\n"),
            ("\u{feff}a = '''\r\n\r\n'''\r\n", "\u{feff}a = '''\n\n'''\n"),
        ] {
            assert_eq!(crlf_to_lf(text), expected, "{text:?}");
        }
    }

    #[test]
    fn a_text_with_a_lone_cr_is_left_as_it_is() {
        for text in [
            "a = 1\r",
            "a = 1\rb = 2\r\n",
            "a = '''x\ry'''\r\n",
            // A lone CR in front of a CRLF would join the LF if the CRLF
            // became one.
            "a = 1\r\r\nb = 2\r\n",
            "a = '''x\r\r\ny'''\r\n",
            "\r\r\n",
        ] {
            assert_eq!(crlf_to_lf(text), text, "{text:?}");
            assert!(
                toml::from_str::<toml::Table>(&crlf_to_lf(text)).is_err(),
                "{text:?} is invalid TOML, and stays so"
            );
        }
    }

    /// What a multi-line string holds in a file saved with CRLF line ends:
    /// the same as in one saved with LF, as it was with toml 0.8.
    #[test]
    fn the_multi_line_strings_of_a_crlf_text_read_with_lf() {
        let lf = concat!(
            "literal = '''\n",
            "one\n",
            "two\n",
            "'''\n",
            "basic = \"\"\"\n",
            "one\n",
            "two\"\"\"\n",
            // A line-ending backslash takes the line break and the blanks
            // after it; the next line break stays.
            "folded = \"\"\"one \\\n",
            "    two\n",
            "three\"\"\"\n",
        );
        let crlf = lf.replace('\n', "\r\n");
        for text in [lf, crlf.as_str()] {
            let table: toml::Table = toml::from_str(&crlf_to_lf(text)).unwrap();

            assert_eq!(table["literal"].as_str(), Some("one\ntwo\n"), "{text:?}");
            assert_eq!(table["basic"].as_str(), Some("one\ntwo"), "{text:?}");
            assert_eq!(table["folded"].as_str(), Some("one two\nthree"), "{text:?}");
        }
    }
}
