//! A `run_script` call as the conversation shows it: the script the model
//! wrote, what its calls did, and what went back to the model.
//!
//! The runtime writes the calls (see `orca_runtime::script`); this module
//! only arranges what it is given. It reads one thing out of that text, the
//! last line, which says how many calls were made and how long the run took.

use orca_runtime::script::shown_line;

pub(crate) const SCRIPT_TOOL: &str = "run_script";

const SCRIPT_TITLE: &str = "Script";
const CALLS_TITLE: &str = "Calls";
const RETURNED_TITLE: &str = "Returned to the model";
const FAILED_TITLE: &str = "Failed";
const CALLS_NOT_KEPT: &str = "Calls are shown only in the session that ran the script";

/// How a script ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScriptEnd {
    pub(crate) completed: bool,
    /// What went back to the model, or why nothing did.
    pub(crate) text: String,
}

/// One script run, as far as it has gone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScriptRun {
    pub(crate) source: Option<String>,
    /// What the runtime wrote of the script's calls, as it arrived.
    pub(crate) calls: String,
    pub(crate) end: Option<ScriptEnd>,
    /// False for a run rebuilt from a saved conversation, which holds no calls.
    pub(crate) calls_kept: bool,
}

impl ScriptRun {
    /// A run this session is watching.
    pub(crate) fn live() -> Self {
        Self {
            source: None,
            calls: String::new(),
            end: None,
            calls_kept: true,
        }
    }

    /// A run read back from a saved conversation.
    pub(crate) fn saved(source: Option<String>, end: ScriptEnd) -> Self {
        Self {
            source,
            calls: String::new(),
            end: Some(end),
            calls_kept: false,
        }
    }

    /// The runtime's last word on the run, such as `4 tool calls · 412ms`.
    pub(crate) fn summary(&self) -> Option<&str> {
        self.calls
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .filter(|line| is_run_summary(line))
    }

    /// Everything the expanded row shows under its first line. Each line
    /// has its control characters replaced, so none can move the terminal
    /// back over what the line starts with.
    pub(crate) fn body(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if let Some(source) = &self.source {
            lines.push(SCRIPT_TITLE.to_string());
            let width = source.lines().count().to_string().len().max(2);
            lines.extend(
                source
                    .lines()
                    .enumerate()
                    .map(|(index, line)| format!("{:>width$} │ {}", index + 1, shown_line(line))),
            );
        }
        if !self.calls_kept {
            lines.push(CALLS_NOT_KEPT.to_string());
        } else {
            // Written clean by the runtime; cleaned again for a stream an
            // older runtime recorded.
            let calls: Vec<String> = self
                .calls
                .lines()
                .filter(|line| !is_run_summary(line))
                .map(shown_line)
                .collect();
            if !calls.is_empty() {
                lines.push(CALLS_TITLE.to_string());
                lines.extend(calls);
            }
        }
        if let Some(end) = &self.end {
            lines.push(if end.completed {
                format!(
                    "{RETURNED_TITLE} · {}",
                    orca_runtime::script::shown_size(end.text.len())
                )
            } else {
                FAILED_TITLE.to_string()
            });
            lines.extend(
                end.text
                    .lines()
                    .map(|line| format!(" {}", shown_line(line))),
            );
        }
        lines.join("\n")
    }
}

/// The `script` a `run_script` request was made with.
pub(crate) fn script_from_arguments(raw_arguments: &str) -> Option<String> {
    let arguments: serde_json::Value = serde_json::from_str(raw_arguments).ok()?;
    arguments.get("script")?.as_str().map(str::to_string)
}

/// Whether `line` of a run's body names a section. Every other line of a
/// body starts with a space or a digit, and holds no carriage return that
/// could draw over that start, so none can be taken for one.
pub(crate) fn is_section_title(line: &str) -> bool {
    line == SCRIPT_TITLE
        || line == CALLS_TITLE
        || line == FAILED_TITLE
        || line == CALLS_NOT_KEPT
        || line.starts_with(RETURNED_TITLE)
}

/// `4 tool calls · 412ms`, and the `4 tool calls` runs recorded before the
/// time was written.
fn is_run_summary(line: &str) -> bool {
    let after_count = line.trim_start_matches(|c: char| c.is_ascii_digit());
    after_count.len() < line.len() && after_count.starts_with(" tool call")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALLS: &str = concat!(
        "  1   grep {\"pattern\":\"TODO\"} · ok · 12ms · 2.1 KiB\n",
        "          src/a.rs:3: // TODO remove\n",
        "  2 ┬ read_file {\"path\":\"a.rs\"} · ok · 3ms · 1.2 KiB\n",
        "  3 └ read_file {\"path\":\"c.rs\"} · failed · 1ms\n",
        "          c.rs does not exist\n",
        "3 tool calls · 412ms\n",
    );

    fn finished(completed: bool, text: &str) -> ScriptRun {
        ScriptRun {
            source: Some("const a = 1;\nreturn a;".to_string()),
            calls: CALLS.to_string(),
            end: Some(ScriptEnd {
                completed,
                text: text.to_string(),
            }),
            calls_kept: true,
        }
    }

    #[test]
    fn a_finished_run_shows_its_script_its_calls_and_what_went_back() {
        let run = finished(true, "2");

        assert_eq!(run.summary(), Some("3 tool calls · 412ms"));
        assert_eq!(
            run.body(),
            concat!(
                "Script\n",
                " 1 │ const a = 1;\n",
                " 2 │ return a;\n",
                "Calls\n",
                "  1   grep {\"pattern\":\"TODO\"} · ok · 12ms · 2.1 KiB\n",
                "          src/a.rs:3: // TODO remove\n",
                "  2 ┬ read_file {\"path\":\"a.rs\"} · ok · 3ms · 1.2 KiB\n",
                "  3 └ read_file {\"path\":\"c.rs\"} · failed · 1ms\n",
                "          c.rs does not exist\n",
                "Returned to the model · 1 B\n",
                " 2",
            )
        );
    }

    #[test]
    fn a_failed_run_says_failed_and_why() {
        let body = finished(false, "Error: boom (script.js:2:7)").body();

        assert!(
            body.ends_with("Failed\n Error: boom (script.js:2:7)"),
            "{body}"
        );
        assert!(!body.contains("Returned to the model"));
    }

    #[test]
    fn a_running_run_shows_what_has_arrived() {
        let run = ScriptRun {
            source: Some("return 1;".to_string()),
            calls: "  1   git_status {} · ok · 4ms · 9 B\n".to_string(),
            ..ScriptRun::live()
        };

        assert_eq!(run.summary(), None, "the last line is not written yet");
        assert_eq!(
            run.body(),
            "Script\n 1 │ return 1;\nCalls\n  1   git_status {} · ok · 4ms · 9 B"
        );
    }

    #[test]
    fn a_run_with_no_calls_has_no_calls_section() {
        let run = ScriptRun {
            source: Some("return 1;".to_string()),
            calls: "0 tool calls · 2ms\n".to_string(),
            end: Some(ScriptEnd {
                completed: true,
                text: "1".to_string(),
            }),
            calls_kept: true,
        };

        assert_eq!(run.summary(), Some("0 tool calls · 2ms"));
        assert_eq!(
            run.body(),
            "Script\n 1 │ return 1;\nReturned to the model · 1 B\n 1"
        );
    }

    #[test]
    fn a_run_read_from_a_saved_conversation_says_where_calls_are_shown() {
        let run = ScriptRun::saved(
            Some("return 1;".to_string()),
            ScriptEnd {
                completed: true,
                text: String::new(),
            },
        );

        assert_eq!(run.summary(), None);
        assert_eq!(
            run.body(),
            concat!(
                "Script\n 1 │ return 1;\n",
                "Calls are shown only in the session that ran the script\n",
                "Returned to the model · 0 B",
            )
        );
    }

    #[test]
    fn a_run_whose_arguments_never_arrived_has_no_script_section() {
        let run = ScriptRun {
            calls: "1 tool call · 3ms\n".to_string(),
            ..ScriptRun::live()
        };

        assert_eq!(run.body(), "");
        assert_eq!(run.summary(), Some("1 tool call · 3ms"));
    }

    #[test]
    fn a_long_script_keeps_its_line_numbers_in_one_column() {
        let source = (1..=120)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let run = ScriptRun {
            source: Some(source),
            ..ScriptRun::live()
        };

        let body = run.body();
        assert!(body.contains("\n  1 │ line1\n"), "{}", &body[..60]);
        assert!(body.ends_with("120 │ line120"));
    }

    #[test]
    fn a_run_recorded_before_the_time_was_written_is_still_summed_up() {
        let run = ScriptRun {
            calls: "grep TODO · ok · 12ms\n1 tool call\n".to_string(),
            ..ScriptRun::live()
        };

        assert_eq!(run.summary(), Some("1 tool call"));
        assert_eq!(run.body(), "Calls\ngrep TODO · ok · 12ms");
    }

    #[test]
    fn the_script_is_read_out_of_the_requests_arguments() {
        assert_eq!(
            script_from_arguments(r#"{"script":"return 1;"}"#).as_deref(),
            Some("return 1;")
        );
        assert_eq!(script_from_arguments(r#"{"script":7}"#), None);
        assert_eq!(script_from_arguments("not json"), None);
        assert_eq!(script_from_arguments("{}"), None);
    }

    #[test]
    fn a_carriage_return_cannot_turn_a_line_into_a_title() {
        let run = ScriptRun::saved(
            Some("x\rScript".to_string()),
            ScriptEnd {
                completed: true,
                text: "x\rScript".to_string(),
            },
        );

        let body = run.body();
        assert_eq!(
            body,
            concat!(
                "Script\n 1 │ x\u{fffd}Script\n",
                "Calls are shown only in the session that ran the script\n",
                "Returned to the model · 8 B\n x\u{fffd}Script",
            )
        );
        // As the row draws it, where a carriage return goes back to the
        // start of the line.
        let drawn = crate::terminal_output::printable_output(&body);
        let titles: Vec<&str> = drawn
            .lines()
            .filter(|line| is_section_title(line))
            .collect();
        assert_eq!(
            titles,
            [
                "Script",
                "Calls are shown only in the session that ran the script",
                "Returned to the model · 8 B",
            ]
        );
    }

    #[test]
    fn control_characters_in_a_script_and_a_result_are_replaced() {
        let run = ScriptRun {
            source: Some("a\x1b[2Jb\tc\u{2028}d\r\nnext".to_string()),
            end: Some(ScriptEnd {
                completed: false,
                text: "\x07bell\x7f\u{9b}".to_string(),
            }),
            ..ScriptRun::live()
        };

        assert_eq!(
            run.body(),
            concat!(
                "Script\n",
                " 1 │ a\u{fffd}[2Jb c\u{fffd}d\n",
                " 2 │ next\n",
                "Failed\n",
                " \u{fffd}bell\u{fffd}\u{fffd}",
            )
        );
    }

    #[test]
    fn clean_calls_are_shown_as_they_came_and_older_ones_are_made_safe() {
        let clean = concat!(
            "  1 ┬ read_file {\"path\":\"a.rs\"} · ok · 3ms · 1.2 KiB\n",
            "          fn main() { │ }\n",
            "          … 30 more lines\n",
            "  2 ├ grep {\"pattern\":\"x\"} · ok · 1ms · 9 B\n",
            "  3 └ glob {\"pattern\":\"*\"} · failed · 1ms\n",
            "          no match\n",
            "3 tool calls · 9ms\n",
        );
        let shown = |calls: &str| {
            ScriptRun {
                calls: calls.to_string(),
                ..ScriptRun::live()
            }
            .body()
        };

        let expected = format!(
            "Calls\n{}",
            clean.lines().take(6).collect::<Vec<_>>().join("\n")
        );
        assert_eq!(shown(clean), expected);
        // A stream recorded by a runtime that wrote previews as they were.
        assert_eq!(
            shown("grep x · ok · 1ms\n          \x1b[31mred\r  9 x · ok · 1ms\n1 tool call\n"),
            "Calls\ngrep x · ok · 1ms\n          \u{fffd}[31mred\u{fffd}  9 x · ok · 1ms"
        );
    }

    #[test]
    fn only_a_title_is_a_title() {
        let body = finished(true, "Script\nCalls\nFailed").body();

        let titles: Vec<&str> = body.lines().filter(|line| is_section_title(line)).collect();
        assert_eq!(titles, ["Script", "Calls", "Returned to the model · 19 B"]);
    }
}
