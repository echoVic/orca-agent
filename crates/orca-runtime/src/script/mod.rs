//! `run_script`: JavaScript the model writes that calls read-only tools
//! itself, so one model round does the work of several.
//!
//! The script runs inside Orca, in a QuickJS created for it and dropped after
//! it ([`engine`]). It has no access of its own: each tool call it makes
//! comes to the runtime, which decides whether to run it and runs it the way
//! it runs a read the model asked for ([`calls`]). Only what the script
//! returns goes back to the model.

pub(crate) mod calls;
pub mod engine;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};
use orca_mcp::McpRegistry;
use serde::Deserialize;

use self::calls::{SCRIPT_TOOL_OUTPUT_BYTES, ScriptToolCalls, ScriptToolExecutor};
use self::engine::{ScriptEngine, ScriptEngineLimits, ScriptOutcome};
use crate::hooks::HookRunner;
use crate::runtime_tool_call::READONLY_TOOL_TIMEOUT_SECS;

#[derive(Deserialize)]
struct RunScriptArguments {
    script: String,
}

pub(crate) struct RunScriptContext<'a, W: io::Write> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) request: &'a ToolRequest,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) emit_deltas: bool,
}

/// Runs one `run_script` call to its result. The result carries what the
/// call wrote for the person watching, to be kept with the conversation.
pub(crate) fn execute_run_script_tool<W: io::Write>(
    context: RunScriptContext<'_, W>,
) -> ToolResult {
    let mut result = run_script(context);
    // A call that ended before its script ran made no calls, and says so.
    result
        .streamed_output
        .get_or_insert_with(|| run_summary(0, Duration::ZERO).into());
    result
}

/// The last line a script's run writes: `4 tool calls · 412ms`.
fn run_summary(made: u32, took: Duration) -> String {
    format!(
        "{made} tool call{} · {}ms\n",
        if made == 1 { "" } else { "s" },
        took.as_millis()
    )
}

/// The most of what a script wrote of its calls that is kept with its
/// result. A script within the default call limit writes about half of
/// this; one allowed many more calls keeps the first of them.
const KEPT_CALLS_BYTES: usize = 256 * 1024;

/// What a script wrote of its calls, as far as it is kept.
#[derive(Default)]
struct KeptCalls {
    text: String,
    lines_left_out: usize,
}

impl KeptCalls {
    fn push(&mut self, line: &str) {
        if self.lines_left_out == 0 && self.text.len() + line.len() < KEPT_CALLS_BYTES {
            self.text.push_str(line);
            self.text.push('\n');
        } else {
            self.lines_left_out += 1;
        }
    }

    /// Everything kept, ending with the run's last line. Lines left out are
    /// counted in a line written as a preview line is, which nothing reads
    /// as a call.
    fn finish(mut self, summary: &str) -> String {
        if self.lines_left_out > 0 {
            self.text.push_str(&format!(
                "          … {} more line{} not kept\n",
                self.lines_left_out,
                if self.lines_left_out == 1 { "" } else { "s" }
            ));
        }
        self.text.push_str(summary);
        self.text
    }
}

fn run_script<W: io::Write>(context: RunScriptContext<'_, W>) -> ToolResult {
    let RunScriptContext {
        config,
        cwd,
        events,
        sink,
        request,
        mcp_registry,
        hooks,
        cancel,
        emit_deltas,
    } = context;
    let settings = &config.tools.script;

    if !settings.enabled {
        return ToolResult::failed(
            request,
            "run_script is turned off; set `enabled = true` under [tools.script] to use it",
            None,
        );
    }
    let script = match serde_json::from_str::<RunScriptArguments>(
        request.raw_arguments.as_deref().unwrap_or("{}"),
    ) {
        Ok(arguments) if !arguments.script.trim().is_empty() => arguments.script,
        _ => return ToolResult::invalid_input(request, "run_script needs a non-empty `script`"),
    };

    if let Some(refusal) = refusal_for_scripts_left_behind(engine::scripts_left_behind()) {
        return ToolResult::failed(request, refusal, None);
    }

    let started = Instant::now();
    let wall_clock = script_wall_clock(settings.timeout_secs);
    let executor = ReadOnlyToolExecutor {
        cwd,
        mcp_registry,
        cancel,
        // Each of the four asks whether to stop, and is told to once the
        // script's time is up.
        deadline: Instant::now() + wall_clock,
    };
    let mut calls = ScriptToolCalls::new(
        &request.id,
        cwd,
        mcp_registry,
        hooks,
        cancel,
        &executor,
        settings.max_tool_calls,
        config.tools.max_read_parallel,
    );
    // What the person watching sees of the script: its calls, as they end.
    // It is also kept with the result, for whoever reads the conversation
    // later.
    let mut kept = KeptCalls::default();
    let mut show = |line: String| {
        kept.push(&line);
        if emit_deltas {
            let _ = sink.emit(events.tool_output_delta(&request.id, &format!("{line}\n")));
        }
    };

    let outcome = ScriptEngine::run(
        &script,
        ScriptEngineLimits {
            wall_clock,
            memory_bytes: settings.memory_limit_bytes(),
        },
        &mut |batch| {
            let answers = calls.answer(batch);
            calls.drain_report().into_iter().for_each(&mut show);
            answers
        },
        Arc::new({
            let cancel = cancel.clone();
            move || cancel.is_cancelled()
        }),
    );
    let summary = run_summary(calls.made(), started.elapsed());
    if emit_deltas {
        let _ = sink.emit(events.tool_output_delta(&request.id, &summary));
    }

    let mut result = match outcome {
        Ok(ScriptOutcome::Completed { output }) => {
            let (output, truncated) =
                truncate_output_with_policy(output, config.tools.output_truncation);
            ToolResult::completed(request, output, truncated)
        }
        Ok(ScriptOutcome::Failed { error }) => ToolResult::failed(request, error, None),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            ToolResult::cancelled(request, "the script was cancelled", None)
        }
        Err(error) => ToolResult::failed(
            request,
            format!("run_script could not run the script: {error}"),
            None,
        ),
    };
    result.streamed_output = Some(kept.finish(&summary).into());
    result
}

/// How many scripts may be left behind at once before no more are started.
/// Each holds a thread, usually a busy one, and up to the memory limit.
const MAX_SCRIPTS_LEFT_BEHIND: usize = 2;
const TOO_MANY_SCRIPTS_LEFT_BEHIND: &str = "run_script cannot start: two earlier scripts were stopped but are still running, each inside a call that has not returned. They end when those calls return or when Orca exits. Call the tools directly instead";

/// Why no script may start while `left_behind` scripts are still running
/// after they were stopped, if that is too many.
fn refusal_for_scripts_left_behind(left_behind: usize) -> Option<&'static str> {
    (left_behind >= MAX_SCRIPTS_LEFT_BEHIND).then_some(TOO_MANY_SCRIPTS_LEFT_BEHIND)
}

/// The longest a script may run, whatever `timeout_secs` says.
const MAX_SCRIPT_WALL_CLOCK_SECS: u64 = 24 * 60 * 60;

/// A size as the person watching reads it: `312 B`, `2.1 KiB`.
pub fn shown_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

/// How far apart a shown line's tab stops are, in characters.
const SHOWN_TAB_WIDTH: usize = 4;

/// One line of what a script did, safe to show. A tab becomes the spaces up
/// to the line's next tab stop, so indentation reads as it was written. Each
/// other control character (C0, DEL, C1), each line or paragraph separator
/// and each character that sets the direction text is drawn in (the bidi
/// embeddings, overrides and isolates) becomes `�`. Nothing is cut and no
/// other character changes.
pub fn shown_line(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    let mut column = 0;
    for character in text.chars() {
        match character {
            '\t' => {
                let spaces = SHOWN_TAB_WIDTH - column % SHOWN_TAB_WIDTH;
                shown.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
                continue;
            }
            '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                shown.push(char::REPLACEMENT_CHARACTER)
            }
            character if character.is_control() => shown.push(char::REPLACEMENT_CHARACTER),
            character => shown.push(character),
        }
        column += 1;
    }
    shown
}

/// The tools a script may call.
pub fn script_tools() -> &'static [&'static str] {
    &calls::SCRIPT_TOOLS
}

/// The most of one tool result a script is handed.
pub fn script_result_limit_bytes() -> usize {
    SCRIPT_TOOL_OUTPUT_BYTES
}

/// How long a script may run: `timeout_secs`, at least a second and at most a
/// day, so that adding it to now cannot overflow.
pub fn script_wall_clock(timeout_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs.clamp(1, MAX_SCRIPT_WALL_CLOCK_SECS))
}

/// Runs a script's tool call the way a read the model asked for is run in a
/// read-only batch: no extra roots, no external tools, the read-only profile.
struct ReadOnlyToolExecutor<'a> {
    cwd: &'a Path,
    mcp_registry: &'a McpRegistry,
    cancel: &'a CancelToken,
    /// When the script that makes the call runs out of time.
    deadline: Instant,
}

impl ScriptToolExecutor for ReadOnlyToolExecutor<'_> {
    fn execute(&self, request: ToolRequest) -> ToolResult {
        orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation_with_profile(
            &request,
            self.cwd,
            &[],
            self.mcp_registry,
            &[],
            ToolOutputTruncation::bytes(SCRIPT_TOOL_OUTPUT_BYTES),
            READONLY_TOOL_TIMEOUT_SECS,
            None,
            orca_core::capability::ExecutionProfile::ReadOnly,
            None,
            || self.stopped(),
        )
    }

    fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shown_line_has_no_control_character_and_keeps_every_other() {
        assert_eq!(
            shown_line("a\x1b[1m\r\n\x00\x7f\u{85}\u{9b}\u{2028}\u{2029}z"),
            "a\u{fffd}[1m\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}z"
        );
        assert_eq!(
            shown_line("a\u{202e}b\u{2066}c\u{2069}\u{202a}"),
            "a\u{fffd}b\u{fffd}c\u{fffd}\u{fffd}"
        );
        let clean = "  2 ┬ read_file {\"path\":\"a\"} · ok · 3ms · 1.2 KiB │ … é 中";
        assert_eq!(shown_line(clean), clean);
    }

    #[test]
    fn the_calls_kept_with_a_result_are_bounded_and_end_with_the_summary() {
        let mut few = KeptCalls::default();
        few.push("  1   grep {} · ok · 1ms · 2 B");
        few.push("          hit");
        assert_eq!(
            few.finish(&run_summary(1, Duration::from_millis(7))),
            "  1   grep {} · ok · 1ms · 2 B\n          hit\n1 tool call · 7ms\n"
        );

        let line = "x".repeat(1023);
        let mut many = KeptCalls::default();
        for _ in 0..300 {
            many.push(&line);
        }
        // A line that would fit is still left out once one was, so what is
        // kept is the start of the run and nothing after a gap.
        many.push("short");
        let kept = many.finish(&run_summary(300, Duration::from_secs(2)));
        assert!(kept.len() <= KEPT_CALLS_BYTES + 64, "{}", kept.len());
        assert_eq!(kept.matches(&line).count(), 256);
        assert!(
            kept.ends_with("          … 45 more lines not kept\n300 tool calls · 2000ms\n"),
            "{}",
            &kept[kept.len() - 80..]
        );
        assert!(!kept.contains("short"));
    }

    #[test]
    fn a_run_that_made_no_calls_says_so() {
        assert_eq!(run_summary(0, Duration::ZERO), "0 tool calls · 0ms\n");
    }

    #[test]
    fn no_script_starts_while_two_stopped_ones_are_still_running() {
        assert_eq!(refusal_for_scripts_left_behind(0), None);
        assert_eq!(refusal_for_scripts_left_behind(1), None);
        for left_behind in [2, 3] {
            assert_eq!(
                refusal_for_scripts_left_behind(left_behind),
                Some(TOO_MANY_SCRIPTS_LEFT_BEHIND)
            );
        }
    }

    #[test]
    fn a_tab_in_a_shown_line_reaches_the_next_tab_stop() {
        assert_eq!(shown_line("\tif (a) {"), "    if (a) {");
        assert_eq!(shown_line("\t\treturn 1;"), "        return 1;");
        assert_eq!(shown_line("a\tb"), "a   b");
        assert_eq!(shown_line("abcd\te"), "abcd    e");
        // A stop is counted in characters, whatever each takes to encode.
        assert_eq!(shown_line("é\u{7}\tz"), "é\u{fffd}  z");
    }

    #[test]
    fn a_scripts_wall_clock_is_at_least_a_second_and_at_most_a_day() {
        assert_eq!(script_wall_clock(0), Duration::from_secs(1));
        assert_eq!(script_wall_clock(60), Duration::from_secs(60));
        assert_eq!(script_wall_clock(u64::MAX), Duration::from_secs(86_400));
        // The largest setting still makes a deadline.
        let _ = Instant::now() + script_wall_clock(u64::MAX);
    }
}
