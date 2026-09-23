//! What a shell tool's result shows in the transcript. The runtime hands the
//! model a JSON envelope around a command's output (task id, state, exit code,
//! cursors, deadlines); a reader wants the output itself, plus only the parts
//! of the envelope that change what the output means.

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
    fn anything_else_is_left_as_it_is() {
        assert_eq!(terminal_output_display("plain command output"), None);
        assert_eq!(
            terminal_output_display(r#"{"output":"no task id or state"}"#),
            None
        );
        assert_eq!(terminal_output_display(r#"["a", "b"]"#), None);
    }
}
