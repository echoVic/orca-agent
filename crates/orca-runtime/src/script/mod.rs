//! `run_script`: JavaScript the model writes that calls read-only tools
//! itself, so one model round does the work of several.
//!
//! The script runs in a confined Node process ([`host`]). It has no access of
//! its own: each tool call it makes comes to the runtime, which decides
//! whether to run it and runs it the way it runs a read the model asked for
//! ([`calls`]). Only what the script returns goes back to the model.

pub(crate) mod calls;
pub mod host;

use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

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
use self::host::{ScriptHost, ScriptHostLimits, ScriptHostOutcome};
use crate::hooks::HookRunner;
use crate::runtime_tool_call::READONLY_TOOL_TIMEOUT_SECS;
use crate::workflow::host::WorkflowHost;

/// The longest line the script host may send: one tool call or the result.
const MAX_SCRIPT_HOST_LINE_BYTES: usize = 1024 * 1024;

/// Whether this machine has a Node to run scripts on. Asked once: the answer
/// decides what a session advertises, and that must not change mid-session.
pub(crate) fn node_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(WorkflowHost::node_available)
}

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
    if !node_available() {
        return ToolResult::failed(
            request,
            "run_script needs Node.js, which was not found on this machine",
            None,
        );
    }
    let script = match serde_json::from_str::<RunScriptArguments>(
        request.raw_arguments.as_deref().unwrap_or("{}"),
    ) {
        Ok(arguments) if !arguments.script.trim().is_empty() => arguments.script,
        _ => return ToolResult::invalid_input(request, "run_script needs a non-empty `script`"),
    };

    let executor = ReadOnlyToolExecutor {
        cwd,
        mcp_registry,
        cancel,
    };
    let mut calls = ScriptToolCalls::new(
        &request.id,
        cwd,
        mcp_registry,
        hooks,
        cancel,
        &executor,
        settings.max_tool_calls,
    );
    // What the person watching sees of the script: its calls, as they end.
    let mut show = |line: String| {
        if emit_deltas {
            let _ = sink.emit(events.tool_output_delta(&request.id, &format!("{line}\n")));
        }
    };

    let outcome = ScriptHost::run(
        &script,
        ScriptHostLimits {
            wall_clock: Duration::from_secs(settings.timeout_secs.max(1)),
            max_line_bytes: MAX_SCRIPT_HOST_LINE_BYTES,
        },
        &mut |call| {
            let answer = calls.answer(call);
            calls.drain_report().into_iter().for_each(&mut show);
            answer
        },
        &|| cancel.is_cancelled(),
    );
    let made = calls.made();
    show(format!(
        "{made} tool call{}",
        if made == 1 { "" } else { "s" }
    ));

    match outcome {
        Ok(ScriptHostOutcome::Completed { output }) => {
            let (output, truncated) =
                truncate_output_with_policy(output, config.tools.output_truncation);
            ToolResult::completed(request, output, truncated)
        }
        Ok(ScriptHostOutcome::Failed { error }) => ToolResult::failed(request, error, None),
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

/// Runs a script's tool call the way a read the model asked for is run in a
/// read-only batch: no extra roots, no external tools, the read-only profile.
struct ReadOnlyToolExecutor<'a> {
    cwd: &'a Path,
    mcp_registry: &'a McpRegistry,
    cancel: &'a CancelToken,
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
            || self.cancel.is_cancelled(),
        )
    }
}
