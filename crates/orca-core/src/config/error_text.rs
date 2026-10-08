//! What a config error says, with nothing the config holds.
//!
//! The errors `toml` and `serde` write about a config quote it: a syntax error
//! prints the line the parser stopped at, a value of the wrong type is printed
//! back (`invalid type: string "TOKEN=abc", expected a map`), and a variant
//! the config does not have is named. A value in a config can be a secret (an
//! environment variable, a header, a token), and what Orca prints about a
//! config lands in logs and bug reports. So these functions say only where
//! the problem is, as a line and column or as a key path, and what was
//! expected.
//!
//! Key names are not values: `orca mcp get` prints them too, and a key path
//! is how a problem is located. Beyond that there is one rule: a message is
//! kept only in the shapes `serde` writes itself, and only in the parts of
//! them that cannot hold a value. A message in any other shape is
//! `serde::de::Error::custom` text, which Orca cannot tell holds no value, so
//! it is not shown.

use std::ops::Range;

/// A TOML syntax error, as where and what: `TOML syntax error at line 1,
/// column 8: string values must be quoted, expected literal string`.
/// `message` and `span` are the error's own, and `source` is the text that
/// was parsed. The parser's messages are fixed text (and key names, for a
/// duplicate key); the line it stopped at is not quoted.
pub fn syntax_error_text(message: &str, span: Option<Range<usize>>, source: &str) -> String {
    let message = message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let message = duplicate_key_message(&message, span.as_ref(), source).unwrap_or(message);
    match span {
        Some(span) => {
            let (line, column) = line_and_column(source, span.start);
            format!("TOML syntax error at line {line}, column {column}: {message}")
        }
        None => format!("TOML syntax error: {message}"),
    }
}

/// The message of a duplicate-key error with the key named, ``duplicate key
/// `name` ``. The parser says only `duplicate key` and spans the key, where
/// the one before it said ``duplicate key `name` in document root``. A key
/// name is not a value, and the old message named it, so it is put back from
/// the span. `None` for any other message, and for a span that holds no key
/// text.
fn duplicate_key_message(
    message: &str,
    span: Option<&Range<usize>>,
    source: &str,
) -> Option<String> {
    if message != "duplicate key" {
        return None;
    }
    let key = source.get(span?.clone())?;
    (!key.is_empty() && !key.contains(['`', '\n', '\r'])).then(|| format!("duplicate key `{key}`"))
}

/// A TOML value that does not load as the type it is read as, as where and
/// what was expected: ``invalid type in `env.PIN`, expected a string``. The
/// value that was found is not shown, and neither is the name of a variant
/// the config does not have; the ones it could have been are. A message that
/// is not in a shape `serde` writes itself is only `invalid value`, with the
/// key path.
///
/// `error` must come from `toml::Value::try_into` (or `toml::Table`'s): only
/// such an error names the key path, below its message. One from parsing
/// text, as `toml::from_str` returns, names no path and quotes the line it
/// stopped at; parse the text to a `toml::Table` first, report a syntax
/// error with [`syntax_error_text`], and then load the table.
pub fn data_error_text(error: &toml::de::Error) -> String {
    let message = error.message();
    let at = key_path(&error.to_string(), message)
        .map(|path| format!(" in `{path}`"))
        .unwrap_or_default();
    if let Some(name) = code_name(message, "missing field `") {
        return format!("missing field `{name}`{at}");
    }
    if let Some(name) = code_name(message, "duplicate field `") {
        return format!("duplicate field `{name}`{at}");
    }
    // `invalid type: string "TOKEN=abc", expected a map`, and the others like
    // it: what was found comes first, what was expected last. A found value
    // can itself contain `, expected `, so the last one is the separator.
    for (start, problem) in [
        ("invalid type: ", "invalid type"),
        ("invalid value: ", "invalid value"),
        ("invalid length ", "invalid length"),
        ("unknown variant `", "unknown variant"),
        ("unknown field `", "unknown field"),
    ] {
        if message.starts_with(start) {
            // An unknown variant or field of a type that has none ends with
            // this instead of what was expected: any `, expected ` before it
            // is the found value's.
            if message.ends_with(", there are no variants")
                || message.ends_with(", there are no fields")
            {
                return format!("{problem}{at}");
            }
            return match message.rsplit_once(", expected ") {
                Some((_, expected)) => format!("{problem}{at}, expected {expected}"),
                None => format!("{problem}{at}"),
            };
        }
    }
    format!("invalid value{at}")
}

/// The key path `serde` adds below a message: Display writes the message, a
/// line break, and then `in` and the path in backticks.
fn key_path(rendered: &str, message: &str) -> Option<String> {
    let path = rendered
        .strip_prefix(message)?
        .trim()
        .strip_prefix("in `")?
        .strip_suffix('`')?;
    (!path.is_empty() && !path.contains('\n')).then(|| path.to_string())
}

/// `name` in a message that is `start`, `name`, and a closing backtick: the
/// name of a field in the code, which the config cannot have chosen.
fn code_name<'a>(message: &'a str, start: &str) -> Option<&'a str> {
    let name = message.strip_prefix(start)?.strip_suffix('`')?;
    (!name.is_empty() && !name.contains(['`', '\n'])).then_some(name)
}

/// The line and the column, both counted from 1, of byte `offset` in
/// `source`. The column counts characters.
fn line_and_column(source: &str, offset: usize) -> (usize, usize) {
    let offset = (0..=offset.min(source.len()))
        .rev()
        .find(|&offset| source.is_char_boundary(offset))
        .unwrap_or(0);
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |text| text.chars().count())
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::Deserialize;
    use serde::de::{Error as _, Unexpected};

    use super::*;

    /// The settings the tests below read, as a config type reads its own.
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Settings {
        name: String,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        mode: Mode,
        #[serde(default)]
        nested: Nested,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        #[default]
        Fast,
        Medium,
        Slow,
    }

    #[derive(Debug, Default, Deserialize)]
    #[allow(dead_code)]
    struct Nested {
        #[serde(default)]
        limit: Option<u8>,
        #[serde(default)]
        required_later: Option<Required>,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Required {
        id: String,
    }

    fn data_error(toml_source: &str) -> toml::de::Error {
        let value: toml::Value = toml::from_str(toml_source).expect("syntactically valid");
        value.try_into::<Settings>().expect_err("does not load")
    }

    #[test]
    fn a_value_of_the_wrong_type_is_not_shown() {
        let cases = [
            (
                "name = \"a\"\nenv = \"TOKEN=abc-SECRET\"\n",
                "invalid type in `env`, expected a map",
            ),
            (
                "name = \"a\"\n[env]\nPIN = 12345678\n",
                "invalid type in `env.PIN`, expected a string",
            ),
            (
                "name = \"a\"\nargs = \"--token=abc-SECRET\"\n",
                "invalid type in `args`, expected a sequence",
            ),
            (
                "name = \"a\"\nargs = [\"ok\", 1.5]\n",
                "invalid type in `args`, expected a string",
            ),
            (
                "name = \"a\"\nport = \"abc-SECRET\"\n",
                "invalid type in `port`, expected u16",
            ),
            (
                "name = [\"abc-SECRET\"]\n",
                "invalid type in `name`, expected a string",
            ),
            ("name = true\n", "invalid type in `name`, expected a string"),
        ];
        for (config, expected) in cases {
            let text = data_error_text(&data_error(config));

            assert_eq!(text, expected, "{config}");
        }
    }

    #[test]
    fn a_value_that_is_out_of_range_is_not_shown() {
        let text = data_error_text(&data_error("name = \"a\"\nport = 70000\n"));

        assert_eq!(text, "invalid value in `port`, expected u16");
        let text = data_error_text(&data_error("name = \"a\"\n[nested]\nlimit = -987654\n"));
        assert_eq!(text, "invalid value in `nested.limit`, expected u8");
    }

    #[test]
    fn the_name_of_a_variant_the_config_does_not_have_is_not_shown() {
        let text = data_error_text(&data_error("name = \"a\"\nmode = \"abc-SECRET\"\n"));

        assert_eq!(
            text,
            "unknown variant in `mode`, expected one of `fast`, `medium`, `slow`"
        );
    }

    #[test]
    fn what_is_missing_is_said_by_the_name_in_the_code() {
        assert_eq!(
            data_error_text(&data_error("env = {}\n")),
            "missing field `name`"
        );
        assert_eq!(
            data_error_text(&data_error("name = \"a\"\n[nested.required_later]\n")),
            "missing field `id` in `nested.required_later`"
        );
    }

    /// A found value that holds the words of the message cannot move where
    /// the message is cut: the last `, expected ` is the separator.
    #[test]
    fn a_value_that_looks_like_the_rest_of_the_message_is_not_shown() {
        for value in [
            ", expected ",
            "abc-SECRET, expected a string",
            "a\", expected a map, expected ",
            "`, expected one of `x`",
            "line one\nline two, expected ",
        ] {
            let found = toml::de::Error::invalid_type(Unexpected::Str(value), &"a map");
            let variant = toml::de::Error::unknown_variant(value, &["fast", "medium", "slow"]);
            // With nothing to expect, serde ends the message without an
            // expected part, so every `, expected ` in it is the value's.
            let no_variant = toml::de::Error::unknown_variant(value, &[]);
            let no_field = toml::de::Error::unknown_field(value, &[]);

            assert_eq!(data_error_text(&found), "invalid type, expected a map");
            assert_eq!(
                data_error_text(&variant),
                "unknown variant, expected one of `fast`, `medium`, `slow`"
            );
            assert_eq!(data_error_text(&no_variant), "unknown variant", "{value}");
            assert_eq!(data_error_text(&no_field), "unknown field", "{value}");
        }
    }

    #[test]
    fn the_other_shapes_serde_writes_keep_only_what_is_expected() {
        let cases = [
            (
                toml::de::Error::invalid_value(Unexpected::Str("abc-SECRET"), &"a short name"),
                "invalid value, expected a short name",
            ),
            (
                toml::de::Error::invalid_length(3, &"fewer elements in map"),
                "invalid length, expected fewer elements in map",
            ),
            (
                toml::de::Error::unknown_field("abc-SECRET", &["name", "env", "args"]),
                "unknown field, expected one of `name`, `env`, `args`",
            ),
            (
                toml::de::Error::unknown_field("abc-SECRET", &["name", "env"]),
                "unknown field, expected `name` or `env`",
            ),
            (
                toml::de::Error::unknown_field("abc-SECRET", &[]),
                "unknown field",
            ),
            (
                toml::de::Error::unknown_variant("abc-SECRET", &[]),
                "unknown variant",
            ),
            (
                toml::de::Error::duplicate_field("name"),
                "duplicate field `name`",
            ),
            (
                toml::de::Error::missing_field("name"),
                "missing field `name`",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(data_error_text(&error), expected);
        }
    }

    /// Text Orca's own types write with `custom` is not known to hold no
    /// value, so none of it is shown.
    #[test]
    fn a_message_in_no_shape_serde_writes_is_not_shown() {
        for message in [
            "unknown tool name: abc-SECRET",
            "vim_insert_escape must contain exactly two characters",
            "missing field `a` and abc-SECRET",
            "missing field `abc`-SECRET`",
            "invalid\ntype: string \"abc-SECRET\", expected a map",
        ] {
            let error = toml::de::Error::custom(message);

            assert_eq!(data_error_text(&error), "invalid value", "{message}");
        }
    }

    #[test]
    fn a_syntax_error_is_the_line_and_column_and_the_parsers_message() {
        let source = "name = \"a\"\nenv = { TOKEN = \"abc-SECRET }\n";
        let error = toml::from_str::<toml::Table>(source).unwrap_err();

        let text = syntax_error_text(error.message(), error.span(), source);

        assert_eq!(
            text,
            "TOML syntax error at line 2, column 30: unclosed inline table, expected `}`"
        );
        assert!(!text.contains("abc-SECRET"), "{text}");
    }

    #[test]
    fn the_lines_of_a_parsers_message_are_joined() {
        let source = "name = abc-SECRET\n";
        let error = toml::from_str::<toml::Table>(source).unwrap_err();

        let text = syntax_error_text(error.message(), error.span(), source);

        assert_eq!(
            text,
            "TOML syntax error at line 1, column 8: string values must be quoted, expected literal string"
        );
        assert!(!text.contains("abc-SECRET"), "{text}");
        // The parser says its message in one line, but a message of several
        // is one still: the lines are joined, and empty ones are dropped.
        assert_eq!(
            syntax_error_text(
                "invalid string\n  expected `\"`, `'`\n\n",
                Some(7..17),
                source
            ),
            "TOML syntax error at line 1, column 8: invalid string; expected `\"`, `'`"
        );
    }

    #[test]
    fn a_duplicate_key_is_reported_by_its_name_and_where_it_is() {
        let source = "name = \"a\"\nname = \"abc-SECRET\"\n";
        let error = toml::from_str::<toml::Table>(source).unwrap_err();

        let text = syntax_error_text(error.message(), error.span(), source);

        assert_eq!(
            text,
            "TOML syntax error at line 2, column 1: duplicate key `name`"
        );
        assert!(!text.contains("abc-SECRET"), "{text}");

        // In a table, in a dotted key, and as a table: the key is the one
        // that repeats.
        for (source, reported) in [
            (
                "[t]\na = 1\na = \"abc-SECRET\"\n",
                "line 3, column 1: duplicate key `a`",
            ),
            (
                "a.b = 1\na.b = \"abc-SECRET\"\n",
                "line 2, column 3: duplicate key `b`",
            ),
            ("[t]\n[t]\n", "line 2, column 2: duplicate key `t`"),
        ] {
            let error = toml::from_str::<toml::Table>(source).unwrap_err();

            let text = syntax_error_text(error.message(), error.span(), source);

            assert_eq!(text, format!("TOML syntax error at {reported}"));
            assert!(!text.contains("abc-SECRET"), "{text}");
        }
    }

    #[test]
    fn a_duplicate_key_that_the_span_does_not_show_is_not_named() {
        // No position, a position outside the text, and one with no key in it.
        assert_eq!(
            syntax_error_text("duplicate key", None, "a = 1\n"),
            "TOML syntax error: duplicate key"
        );
        assert_eq!(
            syntax_error_text("duplicate key", Some(40..41), "a = 1\n"),
            "TOML syntax error at line 2, column 1: duplicate key"
        );
        assert_eq!(
            syntax_error_text("duplicate key", Some(1..1), "a = 1\n"),
            "TOML syntax error at line 1, column 2: duplicate key"
        );
        // A message that says more than that is the parser's own.
        assert_eq!(
            syntax_error_text("duplicate key `a` in document root", Some(0..1), "a = 1\n"),
            "TOML syntax error at line 1, column 1: duplicate key `a` in document root"
        );
    }

    #[test]
    fn a_syntax_error_without_a_position_has_none() {
        assert_eq!(
            syntax_error_text("invalid number", None, "x = 1"),
            "TOML syntax error: invalid number"
        );
    }

    #[test]
    fn a_position_counts_lines_from_one_and_columns_in_characters() {
        let source = "a = 1\r\nbé = \"ü\" x\nlast";
        let at = |text: &str| source.find(text).unwrap();

        assert_eq!(line_and_column(source, 0), (1, 1));
        assert_eq!(line_and_column(source, at("b")), (2, 1));
        // `é` and `ü` are two bytes and one column each.
        assert_eq!(line_and_column(source, at("x")), (2, 10));
        assert_eq!(line_and_column(source, at("last")), (3, 1));
        // The end of the text, and beyond it, and inside a character.
        assert_eq!(line_and_column(source, source.len()), (3, 5));
        assert_eq!(line_and_column(source, source.len() + 40), (3, 5));
        assert_eq!(line_and_column(source, at("é") + 1), (2, 2));
        assert_eq!(line_and_column("", 0), (1, 1));
    }
}
