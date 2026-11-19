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

/// Runs one `run_script` call to its result.
pub(crate) fn execute_run_script_tool<W: io::Write>(
    context: RunScriptContext<'_, W>,
) -> ToolResult {
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

    let wall_clock = script_wall_clock(settings.timeout_secs);
    let executor = ReadOnlyToolExecutor {
        cwd,
        mcp_registry,
        cancel,
        // A tool that asks whether to stop is told to once the script's time
        // is up. Of the four, only `read_file` asks; the others run to their
        // own end, so the script is stopped when such a call returns.
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
    let mut show = |line: String| {
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
    let made = calls.made();
    show(format!(
        "{made} tool call{}",
        if made == 1 { "" } else { "s" }
    ));

    match outcome {
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
    }
}

/// The longest a script may run, whatever `timeout_secs` says.
const MAX_SCRIPT_WALL_CLOCK_SECS: u64 = 24 * 60 * 60;

/// How long a script may run: `timeout_secs`, at least a second and at most a
/// day, so that adding it to now cannot overflow.
fn script_wall_clock(timeout_secs: u64) -> Duration {
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
    fn a_scripts_wall_clock_is_at_least_a_second_and_at_most_a_day() {
        assert_eq!(script_wall_clock(0), Duration::from_secs(1));
        assert_eq!(script_wall_clock(60), Duration::from_secs(60));
        assert_eq!(script_wall_clock(u64::MAX), Duration::from_secs(86_400));
        // The largest setting still makes a deadline.
        let _ = Instant::now() + script_wall_clock(u64::MAX);
    }
}
