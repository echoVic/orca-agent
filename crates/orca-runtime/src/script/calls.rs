//! The tool calls a script makes: which are allowed, how each one is run, and
//! what the person watching is told about them.

use std::path::Path;
use std::time::Instant;

use orca_core::approval_types::ActionKind;
use orca_core::cancel::CancelToken;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;

use super::host::ScriptToolCall;
use crate::hooks::{HookContext, HookRunError, HookRunner};
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
};

/// The tools a script may call. Each reads and none changes anything.
pub(crate) const SCRIPT_TOOLS: [&str; 4] = ["read_file", "glob", "grep", "git_status"];

/// How much of one tool result a script is given. A script filters what it
/// reads, so it gets far more than a model would be shown.
pub(crate) const SCRIPT_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

/// Runs one read-only tool request to its result.
pub(crate) trait ScriptToolExecutor {
    fn execute(&self, request: ToolRequest) -> ToolResult;
}

/// Answers the tool calls of one script.
pub(crate) struct ScriptToolCalls<'a> {
    /// The id of the `run_script` call the script belongs to.
    pub(crate) script_call_id: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) executor: &'a dyn ScriptToolExecutor,
    pub(crate) max_calls: u32,
    made: u32,
    report: Vec<String>,
}

impl<'a> ScriptToolCalls<'a> {
    pub(crate) fn new(
        script_call_id: &'a str,
        cwd: &'a Path,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        executor: &'a dyn ScriptToolExecutor,
        max_calls: u32,
    ) -> Self {
        Self {
            script_call_id,
            cwd,
            mcp_registry,
            hooks,
            cancel,
            executor,
            max_calls,
            made: 0,
            report: Vec::new(),
        }
    }

    /// How many calls the script has been allowed to make.
    pub(crate) fn made(&self) -> u32 {
        self.made
    }

    /// One line for each call answered since the last time this was asked.
    pub(crate) fn drain_report(&mut self) -> Vec<String> {
        std::mem::take(&mut self.report)
    }

    /// Answers one call: `Ok` with the tool's output, or `Err` with why the
    /// script does not get one.
    pub(crate) fn answer(&mut self, call: ScriptToolCall) -> Result<String, String> {
        if self.made >= self.max_calls {
            return Err(format!(
                "the script reached its limit of {} tool calls",
                self.max_calls
            ));
        }
        self.made += 1;
        let started = Instant::now();
        let answer = self.run(&call);
        self.report.push(format!(
            "{} · {} · {}ms",
            describe(&call),
            if answer.is_ok() { "ok" } else { "failed" },
            started.elapsed().as_millis()
        ));
        answer
    }

    fn run(&self, call: &ScriptToolCall) -> Result<String, String> {
        if !SCRIPT_TOOLS.contains(&call.tool.as_str()) {
            return Err(not_a_script_tool(&call.tool));
        }
        let name = ToolName::from_str(&call.tool).ok_or_else(|| not_a_script_tool(&call.tool))?;
        let request = ToolRequest {
            id: format!("{}:{}", self.script_call_id, self.made),
            name,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(call.arguments.to_string()),
        };
        // The list above is what scripts are offered. This is what makes a
        // call safe to run without asking: it holds whatever the list says.
        if !orca_tools::tool_is_available_readonly_concurrent(&request) {
            return Err(not_a_script_tool(&call.tool));
        }

        let invocation =
            prepare_tool_invocation_with_external(&request, 0, u32::MAX, self.mcp_registry, &[]);
        let outcome = self
            .hooks
            .run_with_cancel_result(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: &self.cwd.display().to_string(),
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
                self.cancel,
            )
            .map_err(|error| match error {
                HookRunError::Cancelled(_) => "the pre-tool hook was cancelled".to_string(),
                HookRunError::Failed(error) => format!("pre_tool_use hook blocked tool: {error}"),
            })?;
        let invocation =
            apply_pre_tool_outcome_with_external(invocation, &outcome, self.mcp_registry, &[])
                .map_err(|failure| failure_text(&failure.into_result()))?;

        let result = self.executor.execute(invocation.effective);
        match result.status {
            ToolStatus::Completed => {
                let mut output = result.output.clone().unwrap_or_default();
                if result.truncated {
                    output.push_str("\n[output truncated]");
                }
                Ok(output)
            }
            _ => Err(failure_text(&result)),
        }
    }
}

fn not_a_script_tool(tool: &str) -> String {
    format!(
        "{tool} is not available to scripts; a script can call {}",
        SCRIPT_TOOLS.join(", ")
    )
}

fn failure_text(result: &ToolResult) -> String {
    result
        .error
        .clone()
        .or_else(|| result.output.clone())
        .unwrap_or_else(|| "the tool call failed".to_string())
}

/// A call as one reads it in a list: the tool and what it was pointed at.
fn describe(call: &ScriptToolCall) -> String {
    let pointed_at = ["path", "pattern"]
        .iter()
        .find_map(|key| call.arguments.get(key).and_then(|value| value.as_str()));
    match pointed_at {
        Some(pointed_at) => format!("{} {pointed_at}", call.tool),
        None => call.tool.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use orca_core::hook_types::HookConfig;
    use serde_json::json;

    use super::*;

    /// Records the requests it is given and answers each as told.
    struct Recorder {
        requests: RefCell<Vec<ToolRequest>>,
        fail_with: Option<&'static str>,
    }

    impl Recorder {
        fn answering() -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(error: &'static str) -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                fail_with: Some(error),
            }
        }

        fn ids(&self) -> Vec<String> {
            self.requests
                .borrow()
                .iter()
                .map(|request| request.id.clone())
                .collect()
        }
    }

    impl ScriptToolExecutor for Recorder {
        fn execute(&self, request: ToolRequest) -> ToolResult {
            let result = match self.fail_with {
                Some(error) => ToolResult::failed(&request, error, None),
                None => {
                    ToolResult::completed(&request, format!("ran {}", request.name.as_str()), false)
                }
            };
            self.requests.borrow_mut().push(request);
            result
        }
    }

    fn call(tool: &str, arguments: serde_json::Value) -> ScriptToolCall {
        ScriptToolCall {
            call_id: "call-1".to_string(),
            tool: tool.to_string(),
            arguments,
        }
    }

    struct Fixture {
        cwd: tempfile::TempDir,
        mcp_registry: McpRegistry,
        hooks: HookRunner,
        cancel: CancelToken,
    }

    impl Fixture {
        fn new(hooks: Vec<HookConfig>) -> Self {
            Self {
                cwd: tempfile::tempdir().expect("tempdir"),
                mcp_registry: McpRegistry::default(),
                hooks: HookRunner::new(hooks),
                cancel: CancelToken::new(),
            }
        }

        fn calls<'a>(&'a self, executor: &'a Recorder, max_calls: u32) -> ScriptToolCalls<'a> {
            ScriptToolCalls::new(
                "outer",
                self.cwd.path(),
                &self.mcp_registry,
                &self.hooks,
                &self.cancel,
                executor,
                max_calls,
            )
        }
    }

    #[test]
    fn a_listed_tool_runs_with_the_scripts_arguments_and_a_numbered_id() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let first = calls.answer(call("read_file", json!({ "path": "a.txt" })));
        let second = calls.answer(call("git_status", json!({})));

        assert_eq!(first.as_deref(), Ok("ran read_file"));
        assert_eq!(second.as_deref(), Ok("ran git_status"));
        assert_eq!(executor.ids(), ["outer:1", "outer:2"]);
        let requests = executor.requests.borrow();
        assert_eq!(requests[0].name, ToolName::ReadFile);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(requests[0].raw_arguments.as_ref().unwrap())
                .unwrap(),
            json!({ "path": "a.txt" })
        );
        assert_eq!(calls.made(), 2);
    }

    #[test]
    fn a_tool_outside_the_list_is_refused_without_running() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        for tool in [
            "bash",
            "write_file",
            "edit",
            "web_fetch",
            "run_script",
            "subagent",
            "mcp__docs__search",
            "list_skills",
            "no_such_tool",
        ] {
            let answer = calls.answer(call(tool, json!({ "path": "a.txt", "command": "true" })));
            let error = answer.expect_err(tool);
            assert!(
                error.contains("is not available to scripts"),
                "{tool}: {error}"
            );
            assert!(
                error.contains("read_file, glob, grep, git_status"),
                "{error}"
            );
        }

        assert!(executor.requests.borrow().is_empty());
    }

    #[test]
    fn arguments_the_tool_does_not_accept_are_refused_without_running() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let missing = calls.answer(call("read_file", json!({})));
        let wrong_type = calls.answer(call("grep", json!({ "pattern": 7 })));

        assert!(missing.expect_err("missing path").contains("path"));
        assert!(wrong_type.is_err());
        assert!(executor.requests.borrow().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn a_pre_tool_hook_sees_each_script_call_and_can_block_it() {
        let seen = tempfile::tempdir().expect("tempdir");
        let seen_path = seen.path().join("seen");
        let fixture = Fixture::new(vec![
            HookConfig {
                event: HookEvent::PreToolUse,
                command: format!("printf x >> '{}'", seen_path.display()),
                tool: Some("read_file".to_string()),
            },
            HookConfig {
                event: HookEvent::PreToolUse,
                command: "printf 'not that one' >&2; exit 7".to_string(),
                tool: Some("grep".to_string()),
            },
        ]);
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let read = calls.answer(call("read_file", json!({ "path": "a.txt" })));
        let read_again = calls.answer(call("read_file", json!({ "path": "b.txt" })));
        let blocked = calls.answer(call("grep", json!({ "pattern": "x" })));

        assert!(read.is_ok() && read_again.is_ok());
        assert_eq!(std::fs::read_to_string(&seen_path).unwrap(), "xx");
        let error = blocked.expect_err("the hook blocks grep");
        assert!(error.contains("pre_tool_use hook blocked tool"), "{error}");
        assert_eq!(executor.ids(), ["outer:1", "outer:2"]);
    }

    #[test]
    fn calls_past_the_limit_are_refused() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 2);

        let answers: Vec<_> = (0..4)
            .map(|_| calls.answer(call("git_status", json!({}))))
            .collect();

        assert!(answers[0].is_ok() && answers[1].is_ok());
        for refused in &answers[2..] {
            assert_eq!(
                refused.clone().expect_err("over the limit"),
                "the script reached its limit of 2 tool calls"
            );
        }
        assert_eq!(executor.requests.borrow().len(), 2);
        assert_eq!(calls.made(), 2);
    }

    #[test]
    fn a_failed_tool_rejects_with_its_error() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::failing("a.txt does not exist");
        let mut calls = fixture.calls(&executor, 10);

        let answer = calls.answer(call("read_file", json!({ "path": "a.txt" })));

        assert_eq!(answer.expect_err("the tool failed"), "a.txt does not exist");
    }

    #[test]
    fn each_call_is_reported_once_with_how_it_ended() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let _ = calls.answer(call("read_file", json!({ "path": "src/a.rs" })));
        let _ = calls.answer(call("grep", json!({ "pattern": "TODO", "path": "src" })));
        let _ = calls.answer(call("git_status", json!({})));
        let _ = calls.answer(call("bash", json!({ "command": "true" })));
        let report = calls.drain_report();

        let without_time: Vec<&str> = report
            .iter()
            .map(|line| line.rsplit_once(" · ").expect("a duration").0)
            .collect();
        assert_eq!(
            without_time,
            [
                "read_file src/a.rs · ok",
                "grep src · ok",
                "git_status · ok",
                "bash · failed",
            ]
        );
        assert!(calls.drain_report().is_empty());
    }
}
