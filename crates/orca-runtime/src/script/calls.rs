//! The tool calls a script makes: which are allowed, how each one is run, and
//! what the person watching is told about them.

use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use orca_core::approval_types::ActionKind;
use orca_core::cancel::CancelToken;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;

use super::engine::ScriptToolCall;
use crate::hooks::{HookContext, HookRunError, HookRunner};
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
};

/// The tools a script may call. Each reads and none changes anything.
pub(crate) const SCRIPT_TOOLS: [&str; 4] = ["read_file", "glob", "grep", "git_status"];

/// How much of one tool result a script is given. A script filters what it
/// reads, so it gets far more than a model would be shown.
pub(crate) const SCRIPT_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

/// Runs one read-only tool request to its result. The calls a script started
/// together are run at the same time, so this is called from several threads.
pub(crate) trait ScriptToolExecutor: Sync {
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
    /// How many of the calls started together run at the same time.
    pub(crate) max_parallel: usize,
    made: u32,
    report: Vec<String>,
}

/// One call of a batch, once it has been checked.
enum Checked {
    /// Past the call limit: not counted, not run, not reported.
    OverLimit(String),
    /// Counted, and refused before it ran.
    Refused { error: String, checking: Duration },
    /// Counted and allowed: the request to run.
    Allowed {
        request: ToolRequest,
        checking: Duration,
    },
}

impl<'a> ScriptToolCalls<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        script_call_id: &'a str,
        cwd: &'a Path,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        executor: &'a dyn ScriptToolExecutor,
        max_calls: u32,
        max_parallel: usize,
    ) -> Self {
        Self {
            script_call_id,
            cwd,
            mcp_registry,
            hooks,
            cancel,
            executor,
            max_calls,
            max_parallel,
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

    /// Answers the calls a script started together: for each, in the order
    /// it was started, `Ok` with the tool's output or `Err` with why the
    /// script does not get one.
    pub(crate) fn answer(&mut self, calls: Vec<ScriptToolCall>) -> Vec<Result<String, String>> {
        // Checked one at a time, in the order the script made them: the
        // limit, the list, the schema and the hooks see calls in that order.
        let checked: Vec<Checked> = calls.iter().map(|call| self.check(call)).collect();
        let allowed: Vec<ToolRequest> = checked
            .iter()
            .filter_map(|checked| match checked {
                Checked::Allowed { request, .. } => Some(request.clone()),
                _ => None,
            })
            .collect();
        let mut ran = run_together(self.executor, allowed, self.max_parallel).into_iter();

        let mut answers = Vec::with_capacity(calls.len());
        for (call, checked) in calls.iter().zip(checked) {
            let (answer, took) = match checked {
                Checked::OverLimit(error) => {
                    answers.push(Err(error));
                    continue;
                }
                Checked::Refused { error, checking } => (Err(error), checking),
                Checked::Allowed { checking, .. } => {
                    let (result, running) = ran.next().expect("each allowed call was run");
                    (answer_from(call, &result), checking + running)
                }
            };
            self.report.push(format!(
                "{} · {} · {}ms",
                describe(call),
                if answer.is_ok() { "ok" } else { "failed" },
                took.as_millis()
            ));
            answers.push(answer);
        }
        answers
    }

    fn check(&mut self, call: &ScriptToolCall) -> Checked {
        if self.made >= self.max_calls {
            return Checked::OverLimit(format!(
                "the script reached its limit of {} tool calls",
                self.max_calls
            ));
        }
        self.made += 1;
        let started = Instant::now();
        match self.allowed_request(call) {
            Ok(request) => Checked::Allowed {
                request,
                checking: started.elapsed(),
            },
            Err(error) => Checked::Refused {
                error,
                checking: started.elapsed(),
            },
        }
    }

    /// The request a call becomes if it may run, or why it may not.
    fn allowed_request(&self, call: &ScriptToolCall) -> Result<ToolRequest, String> {
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
        Ok(invocation.effective)
    }
}

/// What a script gets for a call that ran.
fn answer_from(call: &ScriptToolCall, result: &ToolResult) -> Result<String, String> {
    match result.status {
        // A script counts and searches what it is given. Part of a result
        // handed over as the result would give it a wrong answer with no
        // sign that anything is missing, so it gets an error to act on.
        ToolStatus::Completed if result.truncated => Err(format!(
            "{} returned more than {} KiB, the most a script is given at once; ask for less: \
             read_file takes offset and limit, grep takes head_limit and a narrower path",
            call.tool,
            SCRIPT_TOOL_OUTPUT_BYTES / 1024
        )),
        ToolStatus::Completed => Ok(result.output.clone().unwrap_or_default()),
        _ => Err(failure_text(result)),
    }
}

/// Runs the requests at the same time, at most `max_parallel` at once, and
/// returns each one's result and how long it took, in the order given.
fn run_together(
    executor: &dyn ScriptToolExecutor,
    requests: Vec<ToolRequest>,
    max_parallel: usize,
) -> Vec<(ToolResult, Duration)> {
    let run = |request: ToolRequest| {
        let started = Instant::now();
        let result = executor.execute(request);
        (result, started.elapsed())
    };
    let workers = max_parallel.max(1).min(requests.len());
    // A script that awaits each call makes batches of one: no thread for those.
    if workers <= 1 {
        return requests.into_iter().map(run).collect();
    }

    let results: Vec<Mutex<Option<(ToolResult, Duration)>>> =
        requests.iter().map(|_| Mutex::new(None)).collect();
    let waiting = Mutex::new(requests.into_iter().enumerate());
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    // Held only to take the next request, never while one runs.
                    let next = waiting.lock().expect("no worker holds this lock").next();
                    let Some((index, request)) = next else {
                        return;
                    };
                    let ran = run(request);
                    *results[index].lock().expect("each result has one writer") = Some(ran);
                }
            });
        }
    });
    results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .expect("each result has one writer")
                .expect("each request was run")
        })
        .collect()
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    use orca_core::hook_types::HookConfig;
    use serde_json::json;

    use super::*;

    /// Records the requests it is given and answers each as told.
    struct Recorder {
        requests: Mutex<Vec<ToolRequest>>,
        fail_with: Option<&'static str>,
    }

    impl Recorder {
        fn answering() -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(error: &'static str) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                fail_with: Some(error),
            }
        }

        fn requests(&self) -> Vec<ToolRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self
                .requests()
                .into_iter()
                .map(|request| request.id)
                .collect();
            // Calls of one batch finish in any order.
            ids.sort();
            ids
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
            self.requests.lock().unwrap().push(request);
            result
        }
    }

    fn call(tool: &str, arguments: serde_json::Value) -> ScriptToolCall {
        ScriptToolCall {
            tool: tool.to_string(),
            arguments,
        }
    }

    /// Answers one call on its own, as a script that awaits each call does.
    fn one(calls: &mut ScriptToolCalls<'_>, call: ScriptToolCall) -> Result<String, String> {
        calls.answer(vec![call]).remove(0)
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

        fn calls<'a>(
            &'a self,
            executor: &'a dyn ScriptToolExecutor,
            max_calls: u32,
        ) -> ScriptToolCalls<'a> {
            self.calls_at_once(executor, max_calls, 8)
        }

        fn calls_at_once<'a>(
            &'a self,
            executor: &'a dyn ScriptToolExecutor,
            max_calls: u32,
            max_parallel: usize,
        ) -> ScriptToolCalls<'a> {
            ScriptToolCalls::new(
                "outer",
                self.cwd.path(),
                &self.mcp_registry,
                &self.hooks,
                &self.cancel,
                executor,
                max_calls,
                max_parallel,
            )
        }
    }

    #[test]
    fn a_listed_tool_runs_with_the_scripts_arguments_and_a_numbered_id() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let first = one(&mut calls, call("read_file", json!({ "path": "a.txt" })));
        let second = one(&mut calls, call("git_status", json!({})));

        assert_eq!(first.as_deref(), Ok("ran read_file"));
        assert_eq!(second.as_deref(), Ok("ran git_status"));
        assert_eq!(executor.ids(), ["outer:1", "outer:2"]);
        let requests = executor.requests();
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
            let answer = one(
                &mut calls,
                call(tool, json!({ "path": "a.txt", "command": "true" })),
            );
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

        assert!(executor.requests().is_empty());
    }

    #[test]
    fn arguments_the_tool_does_not_accept_are_refused_without_running() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let missing = one(&mut calls, call("read_file", json!({})));
        let wrong_type = one(&mut calls, call("grep", json!({ "pattern": 7 })));

        assert!(missing.expect_err("missing path").contains("path"));
        assert!(wrong_type.is_err());
        assert!(executor.requests().is_empty());
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

        let read = one(&mut calls, call("read_file", json!({ "path": "a.txt" })));
        let read_again = one(&mut calls, call("read_file", json!({ "path": "b.txt" })));
        let blocked = one(&mut calls, call("grep", json!({ "pattern": "x" })));

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
            .map(|_| one(&mut calls, call("git_status", json!({}))))
            .collect();

        assert!(answers[0].is_ok() && answers[1].is_ok());
        for refused in &answers[2..] {
            assert_eq!(
                refused.clone().expect_err("over the limit"),
                "the script reached its limit of 2 tool calls"
            );
        }
        assert_eq!(executor.requests().len(), 2);
        assert_eq!(calls.made(), 2);
    }

    #[test]
    fn a_failed_tool_rejects_with_its_error() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::failing("a.txt does not exist");
        let mut calls = fixture.calls(&executor, 10);

        let answer = one(&mut calls, call("read_file", json!({ "path": "a.txt" })));

        assert_eq!(answer.expect_err("the tool failed"), "a.txt does not exist");
    }

    #[test]
    fn a_result_cut_short_is_an_error_not_a_partial_answer() {
        struct CutShort;
        impl ScriptToolExecutor for CutShort {
            fn execute(&self, request: ToolRequest) -> ToolResult {
                ToolResult::completed(&request, "the first part".to_string(), true)
            }
        }
        let fixture = Fixture::new(Vec::new());
        let mut calls = fixture.calls(&CutShort, 10);

        let answer = one(&mut calls, call("read_file", json!({ "path": "big.rs" })));

        // A script that counts or searches what it read must not be handed
        // part of a file as if it were the file.
        let error = answer.expect_err("a cut result is not an answer");
        assert!(error.contains("256 KiB"), "{error}");
        assert!(
            error.contains("offset") && error.contains("limit"),
            "{error}"
        );
        assert!(!error.contains("the first part"), "{error}");
    }

    #[test]
    fn each_call_is_reported_once_with_how_it_ended() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let _ = one(&mut calls, call("read_file", json!({ "path": "src/a.rs" })));
        let _ = one(
            &mut calls,
            call("grep", json!({ "pattern": "TODO", "path": "src" })),
        );
        let _ = one(&mut calls, call("git_status", json!({})));
        let _ = one(&mut calls, call("bash", json!({ "command": "true" })));
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

    /// Holds every request until `together` of them are running at once.
    struct Gate {
        together: usize,
        running: Mutex<usize>,
        all_here: Condvar,
    }

    impl ScriptToolExecutor for Gate {
        fn execute(&self, request: ToolRequest) -> ToolResult {
            let mut running = self.running.lock().unwrap();
            *running += 1;
            self.all_here.notify_all();
            let (running, waited) = self
                .all_here
                .wait_timeout_while(running, std::time::Duration::from_secs(10), |running| {
                    *running < self.together
                })
                .unwrap();
            drop(running);
            if waited.timed_out() {
                ToolResult::failed(&request, "the other calls never started", None)
            } else {
                ToolResult::completed(&request, request.id.clone(), false)
            }
        }
    }

    /// Answers with the request's id, the earlier the request the later, and
    /// counts how many requests ran at once.
    struct LaterForEarlier {
        running: AtomicUsize,
        most_at_once: AtomicUsize,
    }

    impl LaterForEarlier {
        fn new() -> Self {
            Self {
                running: AtomicUsize::new(0),
                most_at_once: AtomicUsize::new(0),
            }
        }
    }

    impl ScriptToolExecutor for LaterForEarlier {
        fn execute(&self, request: ToolRequest) -> ToolResult {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_at_once.fetch_max(now, Ordering::SeqCst);
            let number: u64 = request.id.rsplit(':').next().unwrap().parse().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(
                60u64.saturating_sub(number * 10),
            ));
            self.running.fetch_sub(1, Ordering::SeqCst);
            ToolResult::completed(&request, request.id.clone(), false)
        }
    }

    fn reads(paths: &[&str]) -> Vec<ScriptToolCall> {
        paths
            .iter()
            .map(|path| call("read_file", json!({ "path": path })))
            .collect()
    }

    #[test]
    fn calls_started_together_run_at_the_same_time() {
        let fixture = Fixture::new(Vec::new());
        let gate = Gate {
            together: 3,
            running: Mutex::new(0),
            all_here: Condvar::new(),
        };
        let mut calls = fixture.calls(&gate, 10);

        // Each call ends only once all three are running: one after another,
        // the first would wait for the others until it gave up.
        let answers = calls.answer(reads(&["a", "b", "c"]));

        assert_eq!(
            answers,
            [
                Ok("outer:1".to_string()),
                Ok("outer:2".to_string()),
                Ok("outer:3".to_string())
            ]
        );
    }

    #[test]
    fn answers_and_report_follow_the_order_the_calls_were_started_in() {
        let fixture = Fixture::new(Vec::new());
        let executor = LaterForEarlier::new();
        let mut calls = fixture.calls(&executor, 10);

        // The first call finishes last.
        let answers = calls.answer(reads(&["a", "b", "c"]));

        assert_eq!(
            answers,
            [
                Ok("outer:1".to_string()),
                Ok("outer:2".to_string()),
                Ok("outer:3".to_string())
            ]
        );
        let reported: Vec<String> = calls
            .drain_report()
            .iter()
            .map(|line| line.split(" · ").next().unwrap().to_string())
            .collect();
        assert_eq!(reported, ["read_file a", "read_file b", "read_file c"]);
    }

    #[test]
    fn no_more_calls_run_at_once_than_the_limit() {
        let fixture = Fixture::new(Vec::new());
        let executor = LaterForEarlier::new();
        let mut calls = fixture.calls_at_once(&executor, 10, 2);

        let answers = calls.answer(reads(&["a", "b", "c", "d", "e", "f"]));

        assert!(answers.iter().all(Result::is_ok), "{answers:?}");
        assert!(executor.most_at_once.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn a_refused_call_does_not_stop_the_others_of_its_batch() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let answers = calls.answer(vec![
            call("read_file", json!({ "path": "a" })),
            call("bash", json!({ "command": "true" })),
            call("read_file", json!({ "path": "b" })),
        ]);

        assert_eq!(answers[0].as_deref(), Ok("ran read_file"));
        assert!(
            answers[1]
                .as_ref()
                .unwrap_err()
                .contains("is not available to scripts")
        );
        assert_eq!(answers[2].as_deref(), Ok("ran read_file"));
        // The refused call was the second the script made, and keeps its number.
        assert_eq!(executor.ids(), ["outer:1", "outer:3"]);
    }

    #[test]
    fn the_call_limit_is_counted_across_a_batch_that_crosses_it() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 2);

        let answers = calls.answer(reads(&["a", "b", "c", "d"]));

        assert!(answers[0].is_ok() && answers[1].is_ok());
        for refused in &answers[2..] {
            assert_eq!(
                refused.clone().unwrap_err(),
                "the script reached its limit of 2 tool calls"
            );
        }
        assert_eq!(executor.requests().len(), 2);
        assert_eq!(calls.made(), 2);
        assert_eq!(
            calls.drain_report().len(),
            2,
            "a call over the limit is not reported"
        );
    }
}
