use std::cell::RefCell;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
fn cancelling_stops_a_running_script() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let result = ScriptEngine::run(
        r#"await tools.read_file({ path: "a" }); while (true) {}"#,
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

    assert_eq!(error_of(outcome), "the script used more than 8 MiB");
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
             missing: [typeof process, typeof require, typeof fetch, typeof setTimeout, typeof setInterval, typeof queueMicrotask, typeof performance, typeof navigator, typeof std, typeof os],
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
