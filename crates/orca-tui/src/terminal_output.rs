//! What a shell tool's result shows in the transcript. The runtime hands the
//! model a JSON envelope around a command's output (task id, state, exit code,
//! cursors, deadlines); a reader wants the output itself, plus only the parts
//! of the envelope that change what the output means.

use std::borrow::Cow;

use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
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
    task_id: String,
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

/// How a shell command that outlived its call is known to have ended,
/// least to most: a restored row nothing reports on, a task list's
/// status, the command's own record (exit code and why it ended).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum EndSource {
    Unknown,
    TaskList,
    Record,
}

/// What a shell result says once its command has ended: the fields
/// `terminal_output_display` reads, and where they came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandEnd {
    pub(crate) source: EndSource,
    state: String,
    return_reason: String,
    termination_reason: Option<String>,
    exit_code: Option<i64>,
}

/// `return_reason` of a result rewritten from a task list's status.
const TASK_LIST_REASON: &str = "task_list";
/// `return_reason` of a restored result nothing reports on any more.
const RESTORED_REASON: &str = "restored_unknown";

/// The states the terminal service gives a command that is no longer
/// running; `interrupted` is one that was running when Orca stopped.
fn is_ended(state: &str) -> bool {
    matches!(
        state,
        "completed" | "failed" | "stopped" | "cancelled" | "interrupted"
    )
}

fn known_end(payload: &TerminalPayload) -> Option<EndSource> {
    if payload.return_reason == RESTORED_REASON {
        Some(EndSource::Unknown)
    } else if !is_ended(&payload.state) {
        None
    } else if payload.return_reason == TASK_LIST_REASON {
        Some(EndSource::TaskList)
    } else {
        Some(EndSource::Record)
    }
}

/// The task a shell result reports on, and how its end is known: `None`
/// while the result still has the command running.
pub(crate) fn reported_task(content: &str) -> Option<(String, Option<EndSource>)> {
    let payload: TerminalPayload = serde_json::from_str(content.trim()).ok()?;
    let known = known_end(&payload);
    Some((payload.task_id, known))
}

/// The ends a result reports: a `bash`, `task_send_input` or
/// `task_read_output` result (all the same envelope), or each task a
/// `task_wait` result lists. Commands still running are left out.
pub(crate) fn reported_ends(content: &str) -> Vec<(String, CommandEnd)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content.trim()) else {
        return Vec::new();
    };
    if let Some(tasks) = value.get("tasks").and_then(serde_json::Value::as_array) {
        return tasks.iter().filter_map(waited_end).collect();
    }
    let Ok(payload) = serde_json::from_value::<TerminalPayload>(value) else {
        return Vec::new();
    };
    let Some(source) = known_end(&payload) else {
        return Vec::new();
    };
    vec![(
        payload.task_id,
        CommandEnd {
            source,
            state: payload.state,
            return_reason: payload.return_reason,
            termination_reason: payload.termination_reason,
            exit_code: payload.exit_code,
        },
    )]
}

/// One task of a `task_wait` result. A shell task appears as the terminal
/// service records it; other tasks have no `termination` and are skipped.
fn waited_end(task: &serde_json::Value) -> Option<(String, CommandEnd)> {
    #[derive(Deserialize)]
    struct WaitedShell {
        task_id: String,
        status: String,
        termination: String,
        #[serde(default)]
        exit_code: Option<i64>,
        #[serde(default)]
        deadline_reached: bool,
    }
    let task = WaitedShell::deserialize(task).ok()?;
    if !is_ended(&task.status) {
        return None;
    }
    let return_reason = if task.deadline_reached {
        "deadline_exceeded"
    } else {
        "terminal_observed"
    };
    Some((
        task.task_id,
        CommandEnd {
            source: EndSource::Record,
            state: task.status,
            return_reason: return_reason.to_string(),
            termination_reason: Some(task.termination),
            exit_code: task.exit_code,
        },
    ))
}

/// What a task list says about a shell task that has ended. It carries no
/// exit code, so a failed command reads `failed` until its record says more.
pub(crate) fn task_list_end(task: &BackgroundTaskSummary) -> Option<(String, CommandEnd)> {
    if task.task_type != TaskType::Shell {
        return None;
    }
    let (state, termination, exit_code) = match task.status {
        TaskStatus::Completed => ("completed", "exited", Some(0)),
        TaskStatus::Failed => ("failed", "failed", None),
        TaskStatus::Stopped => ("stopped", "stopped", None),
        TaskStatus::Cancelled => ("cancelled", "cancelled", None),
        _ => return None,
    };
    Some((
        task.id.clone(),
        CommandEnd {
            source: EndSource::TaskList,
            state: state.to_string(),
            return_reason: TASK_LIST_REASON.to_string(),
            termination_reason: Some(termination.to_string()),
            exit_code,
        },
    ))
}

/// A restored row's command that nothing reports on any more.
pub(crate) fn unknown_end() -> CommandEnd {
    CommandEnd {
        source: EndSource::Unknown,
        state: "unknown".to_string(),
        return_reason: RESTORED_REASON.to_string(),
        termination_reason: Some("state unknown".to_string()),
        exit_code: None,
    }
}

/// `content`, a shell result, saying how its command ended. The output and
/// every other field stay as they were.
pub(crate) fn with_end(content: &str, end: &CommandEnd) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    let fields = value.as_object_mut()?;
    fields.insert("state".to_string(), end.state.clone().into());
    fields.insert(
        "return_reason".to_string(),
        end.return_reason.clone().into(),
    );
    fields.insert(
        "termination_reason".to_string(),
        end.termination_reason.clone().into(),
    );
    fields.insert("exit_code".to_string(), end.exit_code.into());
    serde_json::to_string(&value).ok()
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

    #[test]
    fn writing_the_end_keeps_the_output_as_it_was() {
        let output = format!(
            "{}\x1b[31m红色\x1b[0m tab\there\r\nlast line without newline",
            "构建日志 build log 0123456789\n".repeat(40_000)
        );
        let running = payload(serde_json::json!({
            "state": "running", "return_reason": "yield_elapsed",
            "termination_reason": null, "exit_code": null, "output": output,
        }));
        let (_, end) = task_list_end(
            &serde_json::from_value(serde_json::json!({
                "id": "task-1", "type": "shell", "status": "completed",
                "description": "build", "createdAtMs": 1,
            }))
            .unwrap(),
        )
        .unwrap();
        let written = with_end(&running, &end).unwrap();
        let before: serde_json::Value = serde_json::from_str(&running).unwrap();
        let after: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(after["output"], before["output"]);
        assert_eq!(terminal_output_display(&written).unwrap().note, None);
    }

    #[test]
    fn a_command_interrupted_by_a_restart_has_ended() {
        // As the terminal service reports a task that was running when Orca
        // stopped: its state is `interrupted`, in a result and in a wait.
        let read = payload(serde_json::json!({
            "state": "interrupted", "termination_reason": "interrupted", "exit_code": null,
        }));
        let wait = serde_json::json!({ "tasks": [
            { "task_id": "task-1", "status": "interrupted", "termination": "interrupted", "exit_code": null },
        ]})
        .to_string();
        for result in [read, wait] {
            let ends = reported_ends(&result);
            assert_eq!(ends.len(), 1, "{result}");
            let running = payload(serde_json::json!({
                "state": "running", "return_reason": "yield_elapsed",
                "termination_reason": null, "exit_code": null,
            }));
            assert_eq!(
                terminal_output_display(&with_end(&running, &ends[0].1).unwrap())
                    .unwrap()
                    .note
                    .as_deref(),
                Some("interrupted")
            );
        }
    }

    #[test]
    fn ends_are_read_from_a_result_and_from_each_waited_task() {
        let read = payload(serde_json::json!({ "exit_code": 7, "state": "failed" }));
        let ends = reported_ends(&read);
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].0, "task-1");
        assert_eq!(ends[0].1.source, EndSource::Record);
        assert!(
            reported_ends(&payload(serde_json::json!({
                "state": "running", "termination_reason": null, "exit_code": null,
            })))
            .is_empty()
        );
        let wait = serde_json::json!({ "tasks": [
            { "task_id": "a", "status": "running", "termination": "running", "exit_code": null },
            { "task_id": "b", "status": "failed", "termination": "interrupted", "exit_code": null },
            { "id": "agent-1", "type": "subagent", "status": "completed" },
        ]})
        .to_string();
        let ends = reported_ends(&wait);
        assert_eq!(
            ends.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(
            terminal_output_display(&with_end(&read, &ends[0].1).unwrap())
                .unwrap()
                .note
                .as_deref(),
            Some("interrupted")
        );
    }
}
