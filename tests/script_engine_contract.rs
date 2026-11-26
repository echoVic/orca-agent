use std::cell::RefCell;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use orca_core::tool_types::MAX_TOOL_OUTPUT_BYTES;
use orca_runtime::script::engine::{
    ScriptEngine, ScriptEngineLimits, ScriptOutcome, ScriptToolCall,
};
use serde_json::json;

fn limits() -> ScriptEngineLimits {
    ScriptEngineLimits {
        wall_clock: Duration::from_secs(20),
        memory_bytes: 64 * 1024 * 1024,
    }
}

fn never() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| false)
}

/// Runs `script`, answering every tool call with `answer`, and returns how it
/// ended and the batches of calls it made.
fn run_batches(
    script: &str,
    limits: ScriptEngineLimits,
    mut answer: impl FnMut(&ScriptToolCall) -> Result<String, String>,
) -> (ScriptOutcome, Vec<Vec<ScriptToolCall>>) {
    let batches = RefCell::new(Vec::new());
    let outcome = ScriptEngine::run(
        script,
        limits,
        &mut |calls| {
            let answers = calls.iter().map(&mut answer).collect();
            batches.borrow_mut().push(calls);
            answers
        },
        never(),
    )
    .expect("the script engine ran");
    (outcome, batches.into_inner())
}

fn run(
    script: &str,
    answer: impl FnMut(&ScriptToolCall) -> Result<String, String>,
) -> (ScriptOutcome, Vec<ScriptToolCall>) {
    let (outcome, batches) = run_batches(script, limits(), answer);
    (outcome, batches.into_iter().flatten().collect())
}

fn output_of(outcome: ScriptOutcome) -> String {
    match outcome {
        ScriptOutcome::Completed { output } => output,
        ScriptOutcome::Failed { error } => panic!("script failed: {error}"),
    }
}

fn error_of(outcome: ScriptOutcome) -> String {
    match outcome {
        ScriptOutcome::Failed { error } => error,
        ScriptOutcome::Completed { output } => panic!("script completed: {output}"),
    }
}

#[test]
fn a_script_returns_what_it_computed_from_tool_results() {
    let (outcome, calls) = run(
        r#"const text = await tools.read_file({ path: "a.txt" }); return text.toUpperCase();"#,
        |_| Ok("hello".to_string()),
    );

    assert_eq!(output_of(outcome), "HELLO");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, "read_file");
    assert_eq!(calls[0].arguments, json!({ "path": "a.txt" }));
}

#[test]
fn every_script_tool_reaches_the_runtime_under_its_own_name() {
    let (outcome, calls) = run(
        r#"await tools.read_file({ path: "a" }); await tools.glob({ pattern: "*" });
           await tools.grep({ pattern: "x" }); await tools.git_status(); return Object.keys(tools);"#,
        |_| Ok(String::new()),
    );

    let names: Vec<&str> = calls.iter().map(|call| call.tool.as_str()).collect();
    assert_eq!(names, ["read_file", "glob", "grep", "git_status"]);
    assert_eq!(
        calls[3].arguments,
        json!({}),
        "no arguments is an empty object"
    );
    let offered: Vec<String> = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(offered, ["read_file", "glob", "grep", "git_status"]);
}

#[test]
fn logged_lines_come_before_the_returned_value() {
    let (outcome, _) = run(
        r#"console.log("one", 2, { three: 3 }); return { ok: true };"#,
        |_| Ok(String::new()),
    );

    assert_eq!(
        output_of(outcome),
        "one 2 {\"three\":3}\n{\n  \"ok\": true\n}"
    );
}

#[test]
fn a_script_that_returns_nothing_has_only_its_log() {
    let (logged, _) = run(r#"console.log("x");"#, |_| Ok(String::new()));
    let (silent, _) = run("const unused = 1;", |_| Ok(String::new()));

    assert_eq!(output_of(logged), "x");
    assert_eq!(output_of(silent), "");
}

#[test]
fn a_failed_tool_call_rejects_and_can_be_caught() {
    let (outcome, _) = run(
        r#"try { await tools.read_file({ path: "missing" }); return "read"; }
           catch (error) { return `${error instanceof Error} ${error.message}`; }"#,
        |_| Err("no such file".to_string()),
    );

    assert_eq!(output_of(outcome), "true no such file");
}

#[test]
fn a_thrown_error_fails_the_script_and_says_where() {
    let (outcome, _) = run(
        "console.log(\"before\");\nthrow new Error(\"boom\");",
        |_| Ok(String::new()),
    );

    let error = error_of(outcome);
    assert!(error.contains("Error: boom"), "{error}");
    assert!(error.contains("script.js:2"), "{error}");
    assert!(error.contains("before"), "what was logged is kept: {error}");
}

#[test]
fn a_column_on_the_first_line_is_counted_from_the_start_of_the_script() {
    let (outcome, _) = run("    undefinedName();", |_| Ok(String::new()));

    let error = error_of(outcome);
    assert!(error.contains("ReferenceError"), "{error}");
    assert!(error.contains("(script.js:1:5)"), "{error}");
}

#[test]
fn a_syntax_error_runs_nothing() {
    let (outcome, calls) = run(r#"await tools.read_file({ path: "a" }); return (;"#, |_| {
        Ok(String::new())
    });

    let error = error_of(outcome);
    assert!(error.contains("SyntaxError"), "{error}");
    assert!(calls.is_empty());
}

#[test]
fn calls_started_together_arrive_as_one_batch_in_start_order() {
    let (outcome, batches) = run_batches(
        r#"const all = await Promise.all(["a", "b", "c"].map((path) => tools.read_file({ path })));
           const last = await tools.read_file({ path: "d" });
           return [...all, last].join(",");"#,
        limits(),
        |call| Ok(format!("<{}>", call.arguments["path"].as_str().unwrap())),
    );

    assert_eq!(output_of(outcome), "<a>,<b>,<c>,<d>");
    let paths: Vec<Vec<&str>> = batches
        .iter()
        .map(|batch| {
            batch
                .iter()
                .map(|call| call.arguments["path"].as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(paths, [vec!["a", "b", "c"], vec!["d"]]);
}

#[test]
fn each_answer_resolves_the_call_it_answers() {
    // The second call of the batch fails; the first and third must still get
    // their own answers.
    let (outcome, _) = run(
        r#"const settled = await Promise.allSettled(["a", "b", "c"].map((path) => tools.read_file({ path })));
           return settled.map((one) => one.status === "fulfilled" ? one.value : `! ${one.reason.message}`);"#,
        |call| match call.arguments["path"].as_str().unwrap() {
            "b" => Err("no b".to_string()),
            path => Ok(path.to_uppercase()),
        },
    );

    let settled: Vec<String> = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(settled, ["A", "! no b", "C"]);
}

#[test]
fn arguments_that_are_not_json_reject_the_call() {
    let (outcome, calls) = run(
        r#"const loop = {}; loop.self = loop;
           try { await tools.read_file(loop); return "called"; } catch (error) { return error.name; }"#,
        |_| Ok(String::new()),
    );

    assert_eq!(output_of(outcome), "TypeError");
    assert!(calls.is_empty());
}

#[test]
fn a_script_that_never_returns_is_stopped_at_the_wall_clock() {
    let started = Instant::now();
    let (outcome, _) = run_batches(
        "while (true) {}",
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(400),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    assert_eq!(
        error_of(outcome),
        "the script ran longer than 400ms and was stopped"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_script_stopped_by_the_clock_keeps_its_log() {
    let (outcome, _) = run_batches(
        r#"console.log("first"); console.log("second"); while (true) {}"#,
        ScriptEngineLimits {
            wall_clock: orca_runtime::script::script_wall_clock(0),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    let error = error_of(outcome);
    assert!(error.contains("the script ran longer than"), "{error}");
    assert!(
        error.contains("Logged before it was stopped:\nfirst\nsecond"),
        "{error}"
    );
}

#[test]
fn a_script_over_its_memory_keeps_its_log() {
    let (outcome, _) = run_batches(
        r#"console.log("before");
           const held = []; while (true) { held.push("x".repeat(1024 * 1024) + held.length); }"#,
        ScriptEngineLimits {
            memory_bytes: 8 * 1024 * 1024,
            ..limits()
        },
        |_| Ok(String::new()),
    );

    let error = error_of(outcome);
    assert!(error.contains("the script used more than 8 MiB"), "{error}");
    assert!(
        error.contains("Logged before it was stopped:\nbefore"),
        "{error}"
    );
}

#[test]
fn a_huge_log_line_keeps_its_prefix() {
    let (outcome, _) = run_batches(
        r#"console.log("漢".repeat(1_900_000)); while (true) {}"#,
        ScriptEngineLimits {
            wall_clock: Duration::from_secs(1),
            memory_bytes: 8 * 1024 * 1024,
        },
        |_| Ok(String::new()),
    );
    let error = error_of(outcome);
    assert!(error.contains("ran longer than"), "{error}");
    assert!(error.contains(&"漢".repeat(100)), "{error}");
}

#[test]
fn kept_log_lines_are_bounded() {
    let (outcome, _) = run_batches(
        r#"console.log("first");
           for (let i = 0; i < 20; i++) { console.log("x".repeat(1024 * 1024)); }
           while (true) {}"#,
        ScriptEngineLimits {
            wall_clock: orca_runtime::script::script_wall_clock(0),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    let error = error_of(outcome);
    let prefix = "the script ran longer than 1s and was stopped\n\nLogged before it was stopped:\n";
    assert!(error.starts_with(&format!("{prefix}first")));
    assert!(error.len() <= prefix.len() + MAX_TOOL_OUTPUT_BYTES);
    assert!(error.contains(&"x".repeat(100)));
}

#[test]
fn a_promise_chain_that_never_ends_is_stopped_at_the_wall_clock() {
    let started = Instant::now();
    let (outcome, _) = run_batches(
        "const again = () => Promise.resolve().then(again); again(); await new Promise(() => {});",
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(400),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    assert!(error_of(outcome).contains("ran longer than"));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_regular_expression_that_never_ends_is_stopped_at_the_wall_clock() {
    let started = Instant::now();
    let (outcome, _) = run_batches(
        r#"return /^(a+)+$/.test("a".repeat(40) + "!");"#,
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(400),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    assert!(error_of(outcome).contains("ran longer than"));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_stopped_script_cannot_catch_being_stopped() {
    let (outcome, _) = run_batches(
        "try { while (true) {} } catch (error) { return \"caught\"; } finally { console.log(\"finally\"); }",
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(300),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    assert!(error_of(outcome).contains("ran longer than"));
}

#[test]
fn a_script_waiting_on_nothing_is_told_so() {
    let (outcome, _) = run("await new Promise(() => {});", |_| Ok(String::new()));

    assert_eq!(
        error_of(outcome),
        "the script is waiting on a promise that nothing will resolve"
    );
}

#[test]
fn unbounded_recursion_is_an_error_in_the_script() {
    let (caught, _) = run(
        "const down = () => down(); try { down(); return \"returned\"; } catch (error) { return String(error); }",
        |_| Ok(String::new()),
    );
    let (uncaught, _) = run("const down = () => down(); down();", |_| Ok(String::new()));

    let caught = output_of(caught);
    assert!(caught.contains("call stack"), "{caught}");
    let uncaught = error_of(uncaught);
    assert!(uncaught.contains("call stack"), "{uncaught}");
}

#[test]
fn a_script_over_its_memory_limit_fails_and_names_the_limit() {
    let small = ScriptEngineLimits {
        memory_bytes: 8 * 1024 * 1024,
        ..limits()
    };
    let (grown, _) = run_batches(
        "const held = []; while (true) { held.push(\"x\".repeat(1024 * 1024) + held.length); }",
        small,
        |_| Ok(String::new()),
    );
    let (one_value, _) = run_batches(r#"return "x".repeat(64 * 1024 * 1024);"#, small, |_| {
        Ok(String::new())
    });

    let grown = error_of(grown);
    assert_eq!(grown, "the script used more than 8 MiB");
    let one_value = error_of(one_value);
    assert_eq!(one_value, "the script used more than 8 MiB");
}

#[test]
fn a_large_tool_result_within_the_memory_limit_is_handed_over_whole() {
    let big = "y".repeat(256 * 1024);
    let (outcome, _) = run(
        "return (await tools.read_file({ path: \"big\" })).length;",
        |_| Ok(big.clone()),
    );

    assert_eq!(output_of(outcome), (256 * 1024).to_string());
}

#[test]
fn a_cancelled_script_still_ends_as_interrupted() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let result = ScriptEngine::run(
        r#"console.log("before"); await tools.read_file({ path: "a" }); while (true) {}"#,
        limits(),
        &mut |calls| {
            cancelled.store(true, Ordering::Relaxed);
            calls.iter().map(|_| Ok(String::new())).collect()
        },
        Arc::new({
            let cancelled = Arc::clone(&cancelled);
            move || cancelled.load(Ordering::Relaxed)
        }),
    );

    let error = result.expect_err("a cancelled script is not an outcome");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn cancelling_during_the_last_tool_call_is_still_a_cancelled_script() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let result = ScriptEngine::run(
        r#"return await tools.read_file({ path: "a" });"#,
        limits(),
        &mut |calls| {
            cancelled.store(true, Ordering::Relaxed);
            calls.iter().map(|_| Ok(String::new())).collect()
        },
        Arc::new({
            let cancelled = Arc::clone(&cancelled);
            move || cancelled.load(Ordering::Relaxed)
        }),
    );

    let error = result.expect_err("a cancelled script is not an outcome");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn an_error_that_only_mentions_memory_is_the_scripts_own_error() {
    let (outcome, _) = run(
        r#"throw new Error("out of memory");"#,
        |_| Ok(String::new()),
    );
    // The engine's own refusal reads like this, but none was made.
    let (internal, _) = run(r#"throw new InternalError("out of memory");"#, |_| {
        Ok(String::new())
    });

    let error = error_of(outcome);
    assert!(error.contains("Error: out of memory"), "{error}");
    assert!(!error.contains("used more than"), "{error}");
    let internal = error_of(internal);
    assert!(
        internal.starts_with("InternalError: out of memory (script.js:1:"),
        "{internal}"
    );
}

#[test]
fn running_out_of_memory_while_reporting_an_error_names_the_limit() {
    let small = ScriptEngineLimits {
        memory_bytes: 8 * 1024 * 1024,
        ..limits()
    };
    let (outcome, _) = run_batches(
        r#"for (let i = 0; i < 6; i++) { console.log("x".repeat(1024 * 1024) + i); }
           throw new Error("after the log");"#,
        small,
        |_| Ok(String::new()),
    );

    let error = error_of(outcome);
    let prefix = "the script used more than 8 MiB\n\nLogged before it was stopped:\n";
    assert_eq!(
        error.strip_prefix(prefix),
        Some("x".repeat(MAX_TOOL_OUTPUT_BYTES).as_str())
    );
}

#[test]
fn a_script_that_breaks_the_reporting_path_ends_as_having_broken_the_engine() {
    let (outcome, _) = run(
        r#"console.log("seen");
           Array.prototype.join = () => { throw new Error("no join"); };
           throw new Error("after the log");"#,
        |_| Ok(String::new()),
    );

    assert_eq!(
        error_of(outcome),
        "The script broke the functions it runs on"
    );
}

#[test]
fn a_script_has_the_language_and_its_tools_and_nothing_else() {
    let (outcome, _) = run(
        r#"let imported = "refused";
           try { await import("node:os"); imported = "imported"; } catch (error) {}
           return {
             missing: [typeof process, typeof require, typeof fetch, typeof setTimeout, typeof setInterval, typeof queueMicrotask, typeof performance, typeof navigator, typeof std, typeof os, typeof __orcaLogLine],
             imported,
             added: Object.getOwnPropertyNames(globalThis).filter((name) => !ECMASCRIPT.includes(name)).sort(),
           };"#
            .replace("ECMASCRIPT", ECMASCRIPT_GLOBALS)
            .as_str(),
        |_| Ok(String::new()),
    );

    let seen: serde_json::Value = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(seen["imported"], "refused");
    assert_eq!(seen["added"], json!(["console", "tools"]));
    assert!(
        seen["missing"]
            .as_array()
            .unwrap()
            .iter()
            .all(|kind| kind == "undefined"),
        "{seen}"
    );
}

/// Every name the language itself puts on the global object.
const ECMASCRIPT_GLOBALS: &str = r#"["globalThis","Infinity","NaN","undefined","eval","isFinite","isNaN","parseFloat","parseInt","decodeURI","decodeURIComponent","encodeURI","encodeURIComponent","escape","unescape","AggregateError","Array","ArrayBuffer","AsyncDisposableStack","BigInt","BigInt64Array","BigUint64Array","Boolean","DataView","Date","DisposableStack","Error","EvalError","FinalizationRegistry","Float16Array","Float32Array","Float64Array","Function","Int8Array","Int16Array","Int32Array","Iterator","Map","Number","Object","Promise","Proxy","RangeError","ReferenceError","RegExp","Set","SharedArrayBuffer","String","SuppressedError","Symbol","SyntaxError","TypeError","Uint8Array","Uint8ClampedArray","Uint16Array","Uint32Array","URIError","WeakMap","WeakRef","WeakSet","Atomics","JSON","Math","Reflect","InternalError"]"#;

#[test]
fn a_script_that_replaces_what_it_runs_on_breaks_only_itself() {
    let (replaced, _) = run(
        r#"globalThis.Promise = function () { throw new Error("mine"); };
           JSON.stringify = () => "not json"; JSON.parse = () => 7;
           const text = await tools.read_file({ path: "a" });
           console.log("still", { here: 1 });
           return { text };"#,
        |_| Ok("fine".to_string()),
    );
    let (broken, _) = run(
        r#"Map.prototype.get = () => { throw new Error("mine"); };
           return await tools.read_file({ path: "a" });"#,
        |_| Ok("fine".to_string()),
    );
    let (after, _) = run("return 1 + 1;", |_| Ok(String::new()));

    assert_eq!(
        output_of(replaced),
        "still {\"here\":1}\n{\n  \"text\": \"fine\"\n}"
    );
    assert_eq!(
        error_of(broken),
        "The script broke the functions it runs on"
    );
    assert_eq!(output_of(after), "2");
}

#[test]
fn nothing_is_kept_from_one_script_to_the_next() {
    let (_, _) = run("globalThis.kept = 1; tools.extra = 1;", |_| {
        Ok(String::new())
    });
    let (outcome, _) = run("return [typeof kept, typeof tools.extra];", |_| {
        Ok(String::new())
    });

    assert_eq!(
        output_of(outcome),
        "[\n  \"undefined\",\n  \"undefined\"\n]"
    );
}

#[test]
fn time_spent_in_a_tool_call_counts_toward_the_wall_clock() {
    let (outcome, batches) = run_batches(
        r#"await tools.read_file({ path: "slow" }); await tools.read_file({ path: "never asked" });"#,
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(100),
            ..limits()
        },
        |_| {
            std::thread::sleep(Duration::from_millis(300));
            Ok(String::new())
        },
    );

    assert_eq!(
        error_of(outcome),
        "the script ran longer than 100ms and was stopped"
    );
    assert_eq!(batches.len(), 1);
}

#[test]
fn a_panic_while_answering_is_a_failed_script_and_the_next_one_runs() {
    let outcome = ScriptEngine::run(
        r#"await tools.read_file({ path: "a" });"#,
        limits(),
        &mut |_| panic!("the tool panicked"),
        never(),
    )
    .expect("the script engine ran");
    let (after, _) = run("return 1 + 1;", |_| Ok(String::new()));

    assert_eq!(
        error_of(outcome),
        "run_script failed inside the script engine"
    );
    assert_eq!(output_of(after), "2");
}

/// Says to stop once `after` has passed: a cancel that comes while the
/// script runs, without a thread to send it.
fn cancel_after(after: Duration) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let started = Instant::now();
    Arc::new(move || started.elapsed() >= after)
}

/// Runs `script` with no tool calls and returns how it ended and how long
/// that took.
fn run_timed(
    script: &str,
    limits: ScriptEngineLimits,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
) -> (io::Result<ScriptOutcome>, Duration) {
    let started = Instant::now();
    let result = ScriptEngine::run(
        script,
        limits,
        &mut |calls| calls.iter().map(|_| Ok(String::new())).collect(),
        stop,
    );
    (result, started.elapsed())
}

/// Each script loops inside one of QuickJS's native functions, where the
/// interrupt handler is never asked, and asserts it is still stopped at its
/// wall clock and by a cancel.
fn assert_native_loop_is_stopped(script: &str) {
    let (timed_out, took) = run_timed(
        script,
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(400),
            ..limits()
        },
        never(),
    );
    assert_eq!(
        error_of(timed_out.expect("the script engine ran")),
        "the script ran longer than 400ms and was stopped",
        "{script}"
    );
    assert!(took < Duration::from_secs(5), "{script}: {took:?}");

    let (cancelled, took) = run_timed(script, limits(), cancel_after(Duration::from_millis(300)));
    let error = cancelled.expect_err("a cancelled script is not an outcome");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{script}");
    assert!(took < Duration::from_secs(5), "{script}: {took:?}");

    let (after, _) = run("return 1 + 1;", |_| Ok(String::new()));
    assert_eq!(output_of(after), "2", "after {script}");
}

#[test]
fn a_script_in_a_native_loop_that_never_ends_is_stopped_and_left_behind() {
    // These loops run for hours or longer (`indexOf` at these sizes compares
    // about 2**46 characters), and QuickJS does not ask the interrupt handler
    // inside them. Each script is stopped and its engine thread is left
    // behind, busy, for the rest of this test process: six threads in all.
    for script in [
        r#"return Array.prototype.join.call({ length: 2 ** 53 - 1 }, "");"#,
        "return [].reverse.call({ length: 2 ** 53 - 1 });",
        r#"return "a".repeat(2 ** 24).indexOf("a".repeat(2 ** 23) + "b");"#,
    ] {
        assert_native_loop_is_stopped(script);
    }
}

#[test]
fn a_script_in_a_long_native_loop_is_stopped_at_its_wall_clock() {
    // This one ends after some seconds, and its engine thread with it.
    assert_native_loop_is_stopped(r#"return new Array(2 ** 26).join("").length;"#);
}

#[test]
fn small_allocations_past_the_memory_limit_name_the_limit() {
    let small = ScriptEngineLimits {
        memory_bytes: 8 * 1024 * 1024,
        ..limits()
    };
    // When QuickJS cannot allocate the error itself it throws `null`.
    for script in [
        "const held = []; for (let i = 0; ; i++) held.push({ i });",
        r#"const held = []; for (let n = 0; ; n++) held.push("line " + n);"#,
        r#"let text = ""; for (let i = 0; ; i++) text += "line " + i + "\n";"#,
        r#"const held = new Map(); for (let i = 0; ; i++) held.set("k" + i, [i]);"#,
    ] {
        let (outcome, _) = run_batches(script, small, |_| Ok(String::new()));
        assert_eq!(
            error_of(outcome),
            "the script used more than 8 MiB",
            "{script}"
        );
    }
}

#[test]
fn a_script_that_catches_a_refused_allocation_and_returns_has_completed() {
    let (outcome, _) = run_batches(
        r#"try { const held = []; for (;;) held.push("x".repeat(1024) + held.length); }
           catch (error) {}
           return "recovered";"#,
        ScriptEngineLimits {
            memory_bytes: 8 * 1024 * 1024,
            ..limits()
        },
        |_| Ok(String::new()),
    );

    assert_eq!(output_of(outcome), "recovered");
}

#[test]
fn running_out_of_memory_while_results_are_delivered_names_the_limit() {
    let big = "y".repeat(256 * 1024);
    let (outcome, batches) = run_batches(
        r#"const all = await Promise.all(Array.from({ length: 40 }, (_, i) => tools.read_file({ path: `p${i}` })));
           return all.length;"#,
        ScriptEngineLimits {
            memory_bytes: 8 * 1024 * 1024,
            ..limits()
        },
        |_| Ok(big.clone()),
    );

    assert_eq!(batches.len(), 1);
    assert_eq!(error_of(outcome), "the script used more than 8 MiB");
}

#[test]
fn a_scripts_stack_does_not_depend_on_the_callers() {
    // The engine runs on a thread of its own, so a caller with a small stack
    // still gets a `RangeError` in the script rather than an overflow of its
    // own stack.
    let outcome = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            run(
                "const down = () => down(); try { down(); return \"returned\"; } catch (error) { return String(error); }",
                |_| Ok(String::new()),
            )
            .0
        })
        .expect("a thread with a small stack")
        .join()
        .expect("the caller's thread did not overflow");

    let caught = output_of(outcome);
    assert!(caught.contains("RangeError"), "{caught}");
    assert!(caught.contains("call stack"), "{caught}");
}

#[test]
fn a_script_that_replaces_object_prototype_members_breaks_only_itself() {
    let (unshowable, _) = run(
        r#"Object.prototype.toString = () => { throw new Error("mine"); };
           throw {};"#,
        |_| Ok(String::new()),
    );
    let (never_settles, _) = run(
        r#"Object.prototype.then = function () {};
           return { ok: true };"#,
        |_| Ok(String::new()),
    );
    let (replaced_json, calls) = run(
        r#"Object.prototype.toJSON = function () { return "replaced"; };
           const text = await tools.read_file({ path: "a" });
           return { text };"#,
        |_| Ok("fine".to_string()),
    );
    let (after, _) = run(
        r#"return [await tools.read_file({ path: "a" }), String({}), JSON.stringify({ a: 1 })];"#,
        |_| Ok("fine".to_string()),
    );

    assert_eq!(
        error_of(unshowable),
        "The script threw a value that cannot be shown"
    );
    assert_eq!(
        error_of(never_settles),
        "the script is waiting on a promise that nothing will resolve"
    );
    assert_eq!(output_of(replaced_json), "\"replaced\"");
    assert_eq!(calls[0].arguments, json!("replaced"));
    assert_eq!(
        output_of(after),
        "[\n  \"fine\",\n  \"[object Object]\",\n  \"{\\\"a\\\":1}\"\n]"
    );
}

/// What a call refused for the argument text its waiting calls hold rejects
/// with.
const WAITING_ARGUMENTS_REFUSED: &str = "tool call arguments waiting to be answered are limited to 256 KiB in all; await some calls before starting more";

#[test]
fn a_call_whose_arguments_are_over_256_kib_rejects_in_the_script() {
    // `{"path":"…"}` is the path and 11 bytes more: the first call is 11
    // bytes over, the second exactly at the limit.
    let (outcome, calls) = run(
        r#"let refused;
           try { await tools.read_file({ path: "x".repeat(256 * 1024) }); } catch (error) { refused = `${error instanceof Error} ${error.message}`; }
           const atTheLimit = await tools.read_file({ path: "x".repeat(256 * 1024 - 11) });
           return [refused, atTheLimit];"#,
        |call| Ok(call.arguments["path"].as_str().unwrap().len().to_string()),
    );

    let seen: Vec<String> = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(
        seen,
        [
            format!("true {WAITING_ARGUMENTS_REFUSED}"),
            (256 * 1024 - 11).to_string()
        ]
    );
    assert_eq!(calls.len(), 1, "the refused call is not handed over");
}

#[test]
fn the_arguments_waiting_to_be_handed_over_are_limited_in_all() {
    let (outcome, batches) = run_batches(
        r#"const big = "x".repeat(100 * 1024);
           const settle = (call) => call.then((text) => text, (error) => `${error instanceof Error} ${error.message}`);
           const started = [settle(tools.read_file({ path: big })), settle(tools.read_file({ path: big })), settle(tools.read_file({ path: big }))];
           const settled = await Promise.all(started);
           const later = await settle(tools.read_file({ path: big }));
           return [...settled, later];"#,
        limits(),
        |_| Ok("read".to_string()),
    );

    let seen: Vec<String> = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(
        seen,
        [
            "read".to_string(),
            "read".to_string(),
            format!("true {WAITING_ARGUMENTS_REFUSED}"),
            "read".to_string()
        ]
    );
    let sizes: Vec<usize> = batches.iter().map(Vec::len).collect();
    assert_eq!(sizes, [2, 1], "the third call is not handed over");
    assert!(
        batches
            .iter()
            .flatten()
            .all(|call| call.arguments["path"].as_str().map(str::len) == Some(100 * 1024)),
        "every call handed over is one the script made"
    );
}

#[test]
fn one_call_over_the_limit_is_refused_by_itself() {
    let (outcome, batches) = run_batches(
        r#"try { await tools.read_file({ path: "x".repeat(300 * 1024) }); return "called"; }
           catch (error) { return `${error instanceof Error} ${error.message}`; }"#,
        limits(),
        |_| Ok("read".to_string()),
    );

    assert_eq!(
        output_of(outcome),
        format!("true {WAITING_ARGUMENTS_REFUSED}")
    );
    assert!(batches.is_empty(), "answer is never called: {batches:?}");
}

#[test]
fn what_a_waiting_call_holds_is_its_text() {
    // Each `{"a":0}` and its comma is 8 bytes of text and far more parsed.
    let (outcome, batches) = run_batches(
        r#"const large = tools.read_file({ path: Array(25000).fill({ a: 0 }) });
           const small = tools.read_file({ path: "b" });
           await Promise.all([large, small]);
           return "done";"#,
        limits(),
        |_| Ok("read".to_string()),
    );

    let large = json!({ "path": vec![json!({ "a": 0 }); 25000] });
    let text = serde_json::to_string(&large).unwrap().len();
    assert!((192 * 1024..256 * 1024).contains(&text), "{text} bytes");
    assert_eq!(output_of(outcome), "done");
    assert_eq!(
        batches,
        [vec![
            ScriptToolCall {
                tool: "read_file".to_string(),
                arguments: large,
            },
            ScriptToolCall {
                tool: "read_file".to_string(),
                arguments: json!({ "path": "b" }),
            },
        ]]
    );
}

#[test]
fn too_many_waiting_calls_reject_in_the_script() {
    let (outcome, batches) = run_batches(
        r#"const started = [];
           for (let i = 0; i < 1001; i++) {
             started.push(tools.read_file({ path: "a" }).then(() => "ok", (error) => error.message));
           }
           const settled = await Promise.all(started);
           const later = await tools.read_file({ path: "b" });
           return [settled.filter((one) => one === "ok").length, settled[1000], later];"#,
        limits(),
        |_| Ok("read".to_string()),
    );

    let seen: serde_json::Value = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(
        seen,
        json!([
            1000,
            "too many tool calls are waiting; await some before starting more",
            "read"
        ])
    );
    let sizes: Vec<usize> = batches.iter().map(Vec::len).collect();
    assert_eq!(sizes, [1000, 1]);
}

/// A script that closes the function it is the body of and loops at the top
/// level, where it runs while it is compiled and evaluated.
const BREAKS_OUT_AND_LOOPS: &str = "}); while (true) {} (async function () {";

#[test]
fn a_script_that_breaks_out_of_its_function_is_still_stopped_at_the_wall_clock() {
    let (outcome, took) = run_timed(
        BREAKS_OUT_AND_LOOPS,
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(400),
            ..limits()
        },
        never(),
    );

    assert_eq!(
        error_of(outcome.expect("the script engine ran")),
        "the script ran longer than 400ms and was stopped"
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_script_that_breaks_out_of_its_function_is_still_cancelled() {
    let (result, took) = run_timed(
        BREAKS_OUT_AND_LOOPS,
        limits(),
        cancel_after(Duration::from_millis(300)),
    );

    let error = result.expect_err("a cancelled script is not an outcome");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_script_cannot_replace_how_stacks_are_prepared() {
    // A function set here would be handed every frame beneath an error, the
    // engine's own functions among them.
    let (outcome, _) = run(
        r#"let frames = null;
           Error.prepareStackTrace = (error, callSites) => { frames = callSites; return "replaced"; };
           const stack = new Error("x").stack;
           return [typeof Error.prepareStackTrace, frames === null, stack !== "replaced", stack.includes("script.js")];"#,
        |_| Ok(String::new()),
    );
    let (thrown, _) = run(
        "Error.prepareStackTrace = () => \"replaced\";\nthrow new Error(\"boom\");",
        |_| Ok(String::new()),
    );

    assert_eq!(
        output_of(outcome),
        "[\n  \"undefined\",\n  true,\n  true,\n  true\n]"
    );
    let thrown = error_of(thrown);
    assert!(thrown.starts_with("Error: boom (script.js:2:"), "{thrown}");
}

#[test]
fn returning_ends_the_script_even_while_a_chain_it_started_goes_on() {
    let started = Instant::now();
    let (outcome, _) = run(
        r#"(async () => { for (;;) await 0; })(); return "done";"#,
        |_| Ok(String::new()),
    );

    assert_eq!(output_of(outcome), "done");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_script_that_finishes_after_a_cancel_or_its_time_is_stopped() {
    // Too short for QuickJS to ask the interrupt handler: the script reaches
    // its end, but the cancel, or the end of its time, came first.
    let (cancelled, _) = run_timed("return 1 + 1;", limits(), Arc::new(|| true));
    let (late, _) = run_timed(
        "return 1 + 1;",
        ScriptEngineLimits {
            wall_clock: Duration::ZERO,
            ..limits()
        },
        never(),
    );

    let error = cancelled.expect_err("a cancelled script is not an outcome");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(
        error_of(late.expect("the script engine ran")),
        "the script ran longer than 0s and was stopped"
    );
}

#[test]
fn a_script_that_finishes_just_inside_its_time_is_completed() {
    // What the script holds is freed after it has said how it ended; the time
    // that takes is not the script's.
    let (outcome, _) = run_batches(
        r#"const held = Array.from({ length: 40 }, () => "x".repeat(1024 * 1024));
           return held.length === 40 ? "done" : "not held";"#,
        ScriptEngineLimits {
            wall_clock: Duration::from_secs(5),
            ..limits()
        },
        |_| Ok(String::new()),
    );

    assert_eq!(
        outcome,
        ScriptOutcome::Completed {
            output: "done".to_string()
        }
    );
}
