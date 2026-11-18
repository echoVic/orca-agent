use std::cell::{Cell, RefCell};
use std::io;
use std::time::{Duration, Instant};

use orca_runtime::script::host::{ScriptHost, ScriptHostLimits, ScriptHostOutcome, ScriptToolCall};
use orca_runtime::workflow::host::WorkflowHost;
use serde_json::json;

fn limits() -> ScriptHostLimits {
    ScriptHostLimits {
        wall_clock: Duration::from_secs(20),
        max_line_bytes: 1024 * 1024,
    }
}

/// Runs `script`, answering every tool call with `answer`, and returns how it
/// ended and the calls it made.
fn run(
    script: &str,
    mut answer: impl FnMut(&ScriptToolCall) -> Result<String, String>,
) -> (ScriptHostOutcome, Vec<ScriptToolCall>) {
    let calls = RefCell::new(Vec::new());
    let outcome = ScriptHost::run(
        script,
        limits(),
        &mut |call| {
            let answered = answer(&call);
            calls.borrow_mut().push(call);
            answered
        },
        &|| false,
    )
    .expect("script host ran");
    (outcome, calls.into_inner())
}

fn output_of(outcome: ScriptHostOutcome) -> String {
    match outcome {
        ScriptHostOutcome::Completed { output } => output,
        ScriptHostOutcome::Failed { error } => panic!("script failed: {error}"),
    }
}

fn error_of(outcome: ScriptHostOutcome) -> String {
    match outcome {
        ScriptHostOutcome::Failed { error } => error,
        ScriptHostOutcome::Completed { output } => panic!("script completed: {output}"),
    }
}

#[test]
fn a_script_returns_what_it_computed_from_tool_results() {
    if !WorkflowHost::node_available() {
        return;
    }

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
    if !WorkflowHost::node_available() {
        return;
    }

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
    if !WorkflowHost::node_available() {
        return;
    }

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
    if !WorkflowHost::node_available() {
        return;
    }

    let (logged, _) = run(r#"console.log("x");"#, |_| Ok(String::new()));
    let (silent, _) = run("const unused = 1;", |_| Ok(String::new()));

    assert_eq!(output_of(logged), "x");
    assert_eq!(output_of(silent), "");
}

#[test]
fn a_failed_tool_call_rejects_and_can_be_caught() {
    if !WorkflowHost::node_available() {
        return;
    }

    let (outcome, _) = run(
        r#"try { await tools.read_file({ path: "missing" }); return "read"; }
           catch (error) { return `${error instanceof Error} ${error.message}`; }"#,
        |_| Err("no such file".to_string()),
    );

    assert_eq!(output_of(outcome), "true no such file");
}

#[test]
fn a_thrown_error_fails_the_script_and_says_where() {
    if !WorkflowHost::node_available() {
        return;
    }

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
fn a_syntax_error_runs_nothing() {
    if !WorkflowHost::node_available() {
        return;
    }

    let (outcome, calls) = run(r#"await tools.read_file({ path: "a" }); return (;"#, |_| {
        Ok(String::new())
    });

    assert!(error_of(outcome).contains("SyntaxError"));
    assert!(calls.is_empty());
}

#[test]
fn calls_started_together_are_answered_in_order() {
    if !WorkflowHost::node_available() {
        return;
    }

    let (outcome, calls) = run(
        r#"const all = await Promise.all(["a", "b", "c"].map((path) => tools.read_file({ path })));
           return all.join(",");"#,
        |call| Ok(format!("<{}>", call.arguments["path"].as_str().unwrap())),
    );

    assert_eq!(output_of(outcome), "<a>,<b>,<c>");
    let paths: Vec<&str> = calls
        .iter()
        .map(|call| call.arguments["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["a", "b", "c"]);
}

#[test]
fn a_script_that_never_returns_is_stopped_at_the_wall_clock() {
    if !WorkflowHost::node_available() {
        return;
    }

    let started = Instant::now();
    let outcome = ScriptHost::run(
        "while (true) {}",
        ScriptHostLimits {
            wall_clock: Duration::from_millis(400),
            max_line_bytes: 1024 * 1024,
        },
        &mut |_| Ok(String::new()),
        &|| false,
    )
    .expect("script host ran");

    assert!(error_of(outcome).contains("ran longer than"));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn cancelling_stops_a_running_script() {
    if !WorkflowHost::node_available() {
        return;
    }

    let cancelled = Cell::new(false);
    let started = Instant::now();
    let result = ScriptHost::run(
        r#"await tools.read_file({ path: "a" }); while (true) {}"#,
        limits(),
        &mut |_| {
            cancelled.set(true);
            Ok(String::new())
        },
        &|| cancelled.get(),
    );

    let error = result.expect_err("a cancelled script is not an outcome");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_line_over_the_limit_ends_the_script() {
    if !WorkflowHost::node_available() {
        return;
    }

    let outcome = ScriptHost::run(
        r#"return "x".repeat(4096);"#,
        ScriptHostLimits {
            wall_clock: Duration::from_secs(20),
            max_line_bytes: 1024,
        },
        &mut |_| Ok(String::new()),
        &|| false,
    )
    .expect("script host ran");

    assert!(error_of(outcome).contains("more than 1024 bytes"));
}

#[test]
fn a_script_has_no_node_and_nothing_of_the_hosts() {
    if !WorkflowHost::node_available() {
        return;
    }

    // `makerOf` walks to the function constructor of the realm a value was
    // made in. Every value a script is given must come from its own realm.
    let (outcome, _) = run(
        r#"const key = "construct" + "or";
           const makerOf = (value) => { let maker = value; for (let hop = 0; hop < 3; hop += 1) maker = maker[key]; return maker; };
           const own = makerOf(() => {});
           const failure = await tools.read_file({ path: "missing" }).catch((error) => error);
           const given = { tools, read: tools.read_file, pending: tools.glob({}), answer: await tools.grep({}), failure, console, log: console.log };
           const foreign = Object.keys(given).filter((name) => makerOf(given[name]) !== own);
           let compiled = "refused";
           try { compiled = own("return typeof process")(); } catch (error) {}
           return { foreign, compiled, globals: [typeof process, typeof require, typeof setTimeout, typeof fetch] };"#,
        |call| {
            if call.arguments["path"] == "missing" {
                Err("no such file".to_string())
            } else {
                Ok("text".to_string())
            }
        },
    );

    let seen: serde_json::Value = serde_json::from_str(&output_of(outcome)).unwrap();
    assert_eq!(seen["foreign"], json!([]));
    assert_eq!(seen["compiled"], "refused");
    assert_eq!(
        seen["globals"],
        json!(["undefined", "undefined", "undefined", "undefined"])
    );
}

#[test]
fn a_refused_import_does_not_run_code_in_the_host() {
    if !WorkflowHost::node_available() {
        return;
    }

    // Node answers `import()` with an error made in the host's realm.
    let (outcome, _) = run(
        r#"const key = "construct" + "or";
           try { await import("node:os"); return "imported"; }
           catch (error) {
             try { return error[key][key]("return typeof process")(); }
             catch (inner) { return "refused"; }
           }"#,
        |_| Ok(String::new()),
    );

    assert_eq!(output_of(outcome), "refused");
}
