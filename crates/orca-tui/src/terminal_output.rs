//! What a shell tool's result shows in the transcript. The runtime hands the
//! model a JSON envelope around a command's output (task id, state, exit code,
//! cursors, deadlines); a reader wants the output itself, plus only the parts
//! of the envelope that change what the output means.

use std::borrow::Cow;

use serde::Deserialize;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TerminalOutputDisplay {
    pub(crate) output: String,
    /// "exit 2", "still running", "timed out", ... when the command did not
    /// simply run to a clean exit.
    pub(crate) note: Option<String>,
}

/// The fields that identify a terminal observation, as the runtime's
/// `terminal_output_result` writes it.
#[derive(Deserialize)]
struct TerminalPayload {
    #[serde(rename = "task_id")]
    _task_id: String,
    state: String,
    return_reason: String,
    output: String,
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    termination_reason: Option<String>,
    #[serde(default)]
    output_gap: Option<serde_json::Value>,
}

/// `None` for anything that is not a terminal observation, which then shows
/// as it is.
pub(crate) fn terminal_output_display(content: &str) -> Option<TerminalOutputDisplay> {
    let payload: TerminalPayload = serde_json::from_str(content.trim()).ok()?;
    let mut notes = Vec::new();
    if payload.output_gap.is_some_and(|gap| !gap.is_null()) {
        notes.push("earlier output omitted".to_string());
    }
    if payload.state == "running" {
        notes.push("still running".to_string());
    } else if payload.return_reason == "deadline_exceeded"
        || payload.termination_reason.as_deref() == Some("timed_out")
    {
        notes.push("timed out".to_string());
    } else if let Some(code) = payload.exit_code.filter(|code| *code != 0) {
        notes.push(format!("exit {code}"));
    } else if let Some(reason) = payload
        .termination_reason
        .filter(|reason| reason != "exited")
    {
        notes.push(reason);
    }
    Some(TerminalOutputDisplay {
        output: payload.output,
        note: (!notes.is_empty()).then(|| notes.join(" · ")),
    })
}

/// What a terminal would have shown for `text`, as plain characters. Command
/// output and file contents are drawn through this: an escape sequence
/// (colours, cursor moves, a title, a hyperlink, a clipboard write) is dropped
/// instead of being handed to the user's terminal, a carriage return or
/// backspace overwrites the line the way it would on screen, tabs become
/// spaces, and any other control character is dropped.
pub(crate) fn printable_output(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|character| character != '\n' && character.is_control())
    {
        return Cow::Borrowed(text);
    }
    let mut printed = String::with_capacity(text.len());
    let mut line: Vec<char> = Vec::new();
    let mut column = 0_usize;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\n' => {
                printed.extend(line.drain(..));
                printed.push('\n');
                column = 0;
            }
            '\r' => column = 0,
            '\u{8}' => column = column.saturating_sub(1),
            '\t' => {
                column = (column / TAB_WIDTH + 1) * TAB_WIDTH;
                if line.len() < column {
                    line.resize(column, ' ');
                }
            }
            '\u{1b}' => skip_escape_sequence(&mut characters),
            '\u{9b}' => skip_control_sequence(&mut characters),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                skip_control_string(&mut characters)
            }
            character if character.is_control() => {}
            character => {
                if column < line.len() {
                    line[column] = character;
                } else {
                    line.push(character);
                }
                column += 1;
            }
        }
    }
    printed.extend(line);
    Cow::Owned(printed)
}

const TAB_WIDTH: usize = 8;

type Characters<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// After an ESC.
fn skip_escape_sequence(characters: &mut Characters<'_>) {
    match characters.next() {
        Some('[') => skip_control_sequence(characters),
        Some(']' | 'P' | 'X' | '^' | '_') => skip_control_string(characters),
        // Intermediates, then one final character: ESC ( B.
        Some(' '..='/') => {
            while let Some(&next) = characters.peek() {
                if next.is_control() {
                    return;
                }
                characters.next();
                if !(' '..='/').contains(&next) {
                    return;
                }
            }
        }
        // Two-character sequences such as ESC 7 end with the one taken.
        _ => {}
    }
}

/// A CSI's parameters and intermediates, then its final character.
fn skip_control_sequence(characters: &mut Characters<'_>) {
    while let Some(&next) = characters.peek() {
        match next {
            ' '..='?' => {
                characters.next();
            }
            '@'..='~' => {
                characters.next();
                return;
            }
            _ => return,
        }
    }
}

/// An OSC, DCS, SOS, PM or APC payload, up to its BEL or ST. One left open
/// ends at the line break, so a stray sequence cannot hide the rest of the
/// output.
fn skip_control_string(characters: &mut Characters<'_>) {
    while let Some(&next) = characters.peek() {
        match next {
            '\u{7}' | '\u{9c}' => {
                characters.next();
                return;
            }
            '\n' => return,
            '\u{1b}' => {
                let mut after = characters.clone();
                after.next();
                if after.peek() == Some(&'\\') {
                    characters.next();
                    characters.next();
                }
                return;
            }
            _ => {
                characters.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(fields: serde_json::Value) -> String {
        let mut payload = serde_json::json!({
            "task_id": "task-1",
            "state": "completed",
            "return_reason": "terminal_observed",
            "termination_reason": "exited",
            "exit_code": 0,
            "output": "total 432\ndrwxr-xr-x  41 me  staff  1312 .\n",
            "next_cursor": 3969,
            "output_gap": null,
            "effective_deadline_ms": null,
            "deadline_source": null,
            "terminal": "pipe",
            "eof": true,
        });
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        serde_json::to_string(&payload).unwrap()
    }

    #[test]
    fn a_clean_run_shows_just_its_output() {
        assert_eq!(
            terminal_output_display(&payload(serde_json::json!({}))),
            Some(TerminalOutputDisplay {
                output: "total 432\ndrwxr-xr-x  41 me  staff  1312 .\n".to_string(),
                note: None,
            })
        );
    }

    #[test]
    fn what_changes_the_meaning_of_the_output_is_noted() {
        let note = |fields| terminal_output_display(&payload(fields)).unwrap().note;
        assert_eq!(
            note(serde_json::json!({ "exit_code": 2 })),
            Some("exit 2".into())
        );
        assert_eq!(
            note(serde_json::json!({
                "state": "running",
                "return_reason": "yield_elapsed",
                "termination_reason": null,
                "exit_code": null,
            })),
            Some("still running".into())
        );
        assert_eq!(
            note(serde_json::json!({
                "state": "stopped",
                "return_reason": "deadline_exceeded",
                "termination_reason": "timed_out",
                "exit_code": null,
            })),
            Some("timed out".into())
        );
        assert_eq!(
            note(serde_json::json!({
                "state": "stopped",
                "termination_reason": "cancelled",
                "exit_code": null,
            })),
            Some("cancelled".into())
        );
        assert_eq!(
            note(serde_json::json!({
                "exit_code": 1,
                "output_gap": { "omitted_prefix_bytes": 4096 },
            })),
            Some("earlier output omitted · exit 1".into())
        );
    }

    #[test]
    fn output_is_drawn_as_text_never_as_terminal_commands() {
        for (output, shown) in [
            ("plain text\nsecond line\n", "plain text\nsecond line\n"),
            // A title change, a hyperlink, colours, erase + cursor moves.
            ("before\x1b]2;TITLE\x07after", "beforeafter"),
            ("\x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\", "link"),
            ("\x1b[7;31mRED\x1b[0m done", "RED done"),
            ("rm -rf build\x1b[2K\x1b[1Gls", "rm -rf buildls"),
            // The 8-bit CSI, a DCS payload, and designations like ESC ( B.
            ("\u{9b}31mred", "red"),
            ("\x1bPq#0;2;0;0;0\x1b\\after", "after"),
            ("\x1b(Bplain\x1b7", "plain"),
            // An unterminated clipboard write never reaches the terminal.
            ("x\x1b]52;c;QUJD", "x"),
            ("bell\x07 nul\x00 del\x7f", "bell nul del"),
        ] {
            assert_eq!(printable_output(output), shown, "{output:?}");
        }
    }

    #[test]
    fn carriage_returns_tabs_and_backspaces_show_what_a_terminal_would() {
        for (output, shown) in [
            (
                "downloading 10%\rdownloading 100%\ndone",
                "downloading 100%\ndone",
            ),
            ("abcdef\rXY", "XYcdef"),
            ("line\r\nnext\r\n", "line\nnext\n"),
            ("a\tb", "a       b"),
            ("12345678\tx", "12345678        x"),
            ("ab\x08c", "ac"),
        ] {
            assert_eq!(printable_output(output), shown, "{output:?}");
        }
    }

    #[test]
    fn anything_else_is_left_as_it_is() {
        assert_eq!(terminal_output_display("plain command output"), None);
        assert_eq!(
            terminal_output_display(r#"{"output":"no task id or state"}"#),
            None
        );
        assert_eq!(terminal_output_display(r#"["a", "b"]"#), None);
    }
}
