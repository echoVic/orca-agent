# Script Engine in Process (PTC stage 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run `run_script` scripts in an embedded QuickJS inside Orca instead of a Node process, run the calls a script starts together at the same time, and decide the default from measured results.

**Architecture:** A new `script/engine.rs` creates one QuickJS runtime per script, runs it until it can go no further, hands the waiting tool calls to the runtime as a batch and resumes it with the answers. `script/calls.rs` keeps every check it makes today, in start order, and runs the calls that pass on scoped threads. The Node host, its protocol and everything that existed for them are deleted.

**Tech Stack:** Rust 2024, `rquickjs` 0.14 (QuickJS-NG, built from C source by `cc`), `std::thread::scope`, Python 3 for the comparison runner.

**Spec:** `docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md`

## Global Constraints

- Branch: `feat/script-quickjs-engine`. Baseline `main@47230d46`.
- `rquickjs` is added with its default features only. Never enable `rust-alloc` or `allocator`: with either, the memory limit is silently ignored.
- The tool stays named `run_script`, takes `script`, and offers exactly `read_file`, `glob`, `grep`, `git_status`.
- Limits, verbatim from the spec: wall clock 60 s (`timeout_secs`), 100 inner calls (`max_tool_calls`), 256 KiB per inner result, 64 MiB of memory (`memory_limit_mb`, at least 8), at most `tools.max_read_parallel` calls of a batch at once.
- Failure texts, verbatim: `the script ran longer than {N}s and was stopped`, `the script used more than {N} MiB`, `the script is waiting on a promise that nothing will resolve`, `The script broke the functions it runs on`, `run_script failed inside the script engine`, `the script reached its limit of {N} tool calls`.
- An inner request's id is `<run_script call id>:<n>`, `n` counting from 1 in the order the script started its calls.
- The workflow host (`crates/orca-runtime/src/workflow/`) keeps using Node. Do not change its behaviour.
- Every `cargo` command uses `--locked` once `Cargo.lock` has been updated in Task 1.
- Commit after every task. Do not push; pushing waits for the four validators in Task 6.

## File Structure

| File | Responsibility |
|---|---|
| `crates/orca-runtime/src/script/engine.rs` (new) | One script in one QuickJS: limits, the run-until-waiting loop, outcomes |
| `crates/orca-runtime/src/script/prelude.js` (new) | What a script sees: `tools`, `console`, how a result and an error are put into text |
| `crates/orca-runtime/src/script/calls.rs` | Which calls are allowed, running a batch of them, what the person watching is told |
| `crates/orca-runtime/src/script/mod.rs` | The `run_script` tool: settings in, engine and calls wired, tool result out |
| `crates/orca-runtime/src/script/host.rs`, `host.mjs` (deleted) | The Node host |
| `tests/script_engine_contract.rs` (new, replaces `tests/script_host_contract.rs`) | What a script can and cannot do, and every way it is stopped |
| `crates/orca-core/src/config/mod.rs` | `memory_limit_mb` |
| `scripts/eval/code_mode/expected.py` (new) | The right answer to each comparison task, computed from the checkout |

---

### Task 1: The engine

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`)
- Modify: `crates/orca-runtime/Cargo.toml` (`[dependencies]`)
- Modify: `crates/orca-runtime/src/script/mod.rs:9-10`
- Create: `crates/orca-runtime/src/script/engine.rs`
- Create: `crates/orca-runtime/src/script/prelude.js`
- Test: `tests/script_engine_contract.rs`

**Interfaces:**
- Consumes: nothing.
- Produces, in `orca_runtime::script::engine`:
  - `pub struct ScriptEngineLimits { pub wall_clock: Duration, pub memory_bytes: usize }`
  - `pub struct ScriptToolCall { pub tool: String, pub arguments: serde_json::Value }`
  - `pub enum ScriptOutcome { Completed { output: String }, Failed { error: String } }`
  - `ScriptEngine::run(script: &str, limits: ScriptEngineLimits, answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>, stop: Arc<dyn Fn() -> bool + Send + Sync>) -> io::Result<ScriptOutcome>`

The Node host stays in place and in use until Task 3. This task only adds.

- [ ] **Step 1: Record the release binary's size before anything changes**

Task 6 reports the size after. Run:

```bash
cargo build --release --locked
stat -f%z target/release/orca > /tmp/orca-size-before.txt   # Linux: stat -c%s
cat /tmp/orca-size-before.txt
```

Expected: one number of bytes.

- [ ] **Step 2: Add the dependency**

In `Cargo.toml`, under `[workspace.dependencies]`, add:

```toml
# QuickJS, for `run_script`. Default features only: `rust-alloc` and
# `allocator` turn the engine's memory limit into a no-op.
rquickjs = "0.14"
```

In `crates/orca-runtime/Cargo.toml`, under `[dependencies]`, add:

```toml
rquickjs = { workspace = true }
```

- [ ] **Step 3: Write the failing tests**

Create `tests/script_engine_contract.rs`:

```rust
use std::cell::RefCell;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use orca_runtime::script::engine::{ScriptEngine, ScriptEngineLimits, ScriptOutcome, ScriptToolCall};
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
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test --test script_engine_contract`
Expected: FAIL to compile with `could not find `engine` in `script``. (This run also writes `rquickjs` into `Cargo.lock`.)

- [ ] **Step 5: Write the prelude**

Create `crates/orca-runtime/src/script/prelude.js`:

```javascript
(function (startCall, finish, firstLineShift) {
  "use strict";

  // Taken before the script runs: a script that replaces these breaks only
  // what it does with them itself.
  const { stringify } = JSON;
  const RealmPromise = Promise;
  const RealmError = Error;
  const RealmTypeError = TypeError;
  const waiting = new Map();
  const logged = [];

  function callTool(tool, args) {
    return new RealmPromise((resolve, reject) => {
      const text = stringify(args ?? {});
      if (typeof text !== "string") {
        throw new RealmTypeError(`${tool} takes an object of arguments`);
      }
      waiting.set(startCall(tool, text), { resolve, reject });
    });
  }

  const tools = {};
  for (const tool of ["read_file", "glob", "grep", "git_status"]) {
    tools[tool] = (args) => callTool(tool, args);
  }

  // One value as text: a string as it is, anything else as JSON.
  function show(value, indent) {
    if (typeof value === "string") {
      return value;
    }
    try {
      return stringify(value, null, indent) ?? String(value);
    } catch {
      return String(value);
    }
  }

  function log(...values) {
    logged.push(values.map((value) => show(value)).join(" "));
  }

  // An error as the model should read it: what it is, and where in the script.
  // The script's first line follows the opening of the function it is the body
  // of, so a column on that line is counted from where the script starts.
  function describe(error) {
    try {
      const where = /script\.js:(\d+)(?::(\d+))?/.exec(String(error?.stack ?? ""));
      if (!where) {
        return String(error);
      }
      const line = Number(where[1]);
      if (where[2] === undefined) {
        return `${String(error)} (script.js:${line})`;
      }
      const column = Number(where[2]) - (line === 1 ? firstLineShift : 0);
      return `${String(error)} (script.js:${line}:${column})`;
    } catch {
      return "The script threw a value that cannot be shown";
    }
  }

  // The engine's build of the language adds this one name of its own.
  delete globalThis.queueMicrotask;
  globalThis.tools = tools;
  globalThis.console = { log, info: log, warn: log, error: log, debug: log };

  return {
    // Runs the script and says how it ended.
    async run(main) {
      let kind;
      let text;
      try {
        const value = await main();
        const parts = value === undefined ? logged : [...logged, show(value, 2)];
        kind = "completed";
        text = parts.join("\n");
      } catch (error) {
        const output = logged.length > 0 ? `\n\nLogged before the error:\n${logged.join("\n")}` : "";
        kind = "failed";
        text = `${describe(error)}${output}`;
      }
      finish(kind, text);
    },

    // Hands a tool call its answer.
    deliver(number, ok, text) {
      const call = waiting.get(number);
      if (call === undefined) {
        return;
      }
      waiting.delete(number);
      if (ok) {
        call.resolve(text);
      } else {
        call.reject(new RealmError(text));
      }
    },

    describe,
  };
})
```

- [ ] **Step 6: Write the engine**

Create `crates/orca-runtime/src/script/engine.rs`:

```rust
//! Runs one script in an embedded QuickJS: created for the script, dropped
//! after it. The engine runs no tool. It runs the script until the script can
//! go no further, hands over the calls that are waiting, and resumes it with
//! their answers.

use std::cell::RefCell;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::{Context, Ctx, Function, Object, Runtime, Value as JsValue};
use serde_json::Value;

const PRELUDE: &str = include_str!("prelude.js");
/// The name a script's errors give for where they happened.
const SCRIPT_FILE: &str = "script.js";
/// A script is the body of this function. It opens on the script's first
/// line, so the lines of an error are the lines of the script.
const MAIN_OPEN: &str = "(async function main() {";
const MAIN_CLOSE: &str = "\n})";

const RUNNING: u8 = 0;
const CANCELLED: u8 = 1;
const OUT_OF_TIME: u8 = 2;

/// What stops a script that the script itself does not end.
#[derive(Clone, Copy, Debug)]
pub struct ScriptEngineLimits {
    /// How long the script may run, tool calls included.
    pub wall_clock: Duration,
    /// How much memory the script may hold.
    pub memory_bytes: usize,
}

/// One tool call a script made.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptToolCall {
    pub tool: String,
    pub arguments: Value,
}

/// How a script ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptOutcome {
    Completed { output: String },
    Failed { error: String },
}

/// What the script's side of the engine has told the Rust side.
#[derive(Default)]
struct Told {
    /// Calls started since the last hand-over, each with its number.
    started: Vec<(u32, ScriptToolCall)>,
    numbered: u32,
    outcome: Option<ScriptOutcome>,
}

pub struct ScriptEngine;

impl ScriptEngine {
    /// Runs `script` to its end. `answer` is given the calls the script
    /// started together and returns one answer for each, in the same order:
    /// `Ok(text)` is what a call resolves to, `Err(text)` what it rejects
    /// with. A script stopped because `stop` said so is an `Interrupted`
    /// error, not an outcome.
    pub fn run(
        script: &str,
        limits: ScriptEngineLimits,
        answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
        stop: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> io::Result<ScriptOutcome> {
        match catch_unwind(AssertUnwindSafe(|| run(script, limits, answer, stop))) {
            Ok(result) => result,
            Err(_) => Ok(ScriptOutcome::Failed {
                error: "run_script failed inside the script engine".to_string(),
            }),
        }
    }
}

fn run(
    script: &str,
    limits: ScriptEngineLimits,
    answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
) -> io::Result<ScriptOutcome> {
    let runtime = Runtime::new().map_err(engine_error)?;
    runtime.set_memory_limit(limits.memory_bytes);

    // Why the script was stopped, if it was. Once set, the engine refuses to
    // run anything more of the script.
    let stopped = Arc::new(AtomicU8::new(RUNNING));
    let deadline = Instant::now() + limits.wall_clock;
    runtime.set_interrupt_handler(Some(Box::new({
        let stopped = Arc::clone(&stopped);
        move || {
            if stopped.load(Ordering::Relaxed) != RUNNING {
                return true;
            }
            if stop() {
                stopped.store(CANCELLED, Ordering::Relaxed);
            } else if Instant::now() >= deadline {
                stopped.store(OUT_OF_TIME, Ordering::Relaxed);
            } else {
                return false;
            }
            true
        }
    })));

    let context = Context::custom::<(
        intrinsic::Date,
        intrinsic::Eval,
        intrinsic::RegExpCompiler,
        intrinsic::RegExp,
        intrinsic::Json,
        intrinsic::Proxy,
        intrinsic::MapSet,
        intrinsic::TypedArrays,
        intrinsic::Promise,
        intrinsic::WeakRef,
    )>(&runtime)
    .map_err(engine_error)?;
    let outcome = context.with(|ctx| drive(&ctx, script, limits, answer, &stopped, deadline));
    // Nothing of the script may outlive this call.
    drop(context);
    drop(runtime);
    outcome
}

fn drive<'js>(
    ctx: &Ctx<'js>,
    script: &str,
    limits: ScriptEngineLimits,
    answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
    stopped: &AtomicU8,
    deadline: Instant,
) -> io::Result<ScriptOutcome> {
    let told = Rc::new(RefCell::new(Told::default()));

    let start_call = Function::new(ctx.clone(), {
        let told = Rc::clone(&told);
        move |tool: String, arguments: String| -> u32 {
            let mut told = told.borrow_mut();
            told.numbered += 1;
            let number = told.numbered;
            let arguments = serde_json::from_str(&arguments).unwrap_or(Value::Null);
            told.started
                .push((number, ScriptToolCall { tool, arguments }));
            number
        }
    })
    .map_err(engine_error)?;
    let finish = Function::new(ctx.clone(), {
        let told = Rc::clone(&told);
        move |kind: String, text: String| {
            told.borrow_mut().outcome = Some(if kind == "completed" {
                ScriptOutcome::Completed { output: text }
            } else {
                ScriptOutcome::Failed { error: text }
            });
        }
    })
    .map_err(engine_error)?;

    let install: Function = ctx.eval(PRELUDE).map_err(engine_error)?;
    let api: Object = install
        .call((start_call, finish, MAIN_OPEN.len()))
        .map_err(engine_error)?;
    let run: Function = api.get("run").map_err(engine_error)?;
    let deliver: Function = api.get("deliver").map_err(engine_error)?;
    let describe: Function = api.get("describe").map_err(engine_error)?;

    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(SCRIPT_FILE.to_string());
    let main: Function =
        match ctx.eval_with_options(format!("{MAIN_OPEN}{script}{MAIN_CLOSE}"), options) {
            Ok(main) => main,
            // The script does not parse. Nothing of it ran.
            Err(_) => {
                let error = describe
                    .call::<_, String>((ctx.catch(),))
                    .unwrap_or_else(|_| "the script could not be compiled".to_string());
                return Ok(ScriptOutcome::Failed { error });
            }
        };
    if run.call::<_, JsValue>((main,)).is_err() {
        return ended_early(ctx, stopped, limits);
    }

    loop {
        while ctx.execute_pending_job() {
            if stopped.load(Ordering::Relaxed) != RUNNING {
                break;
            }
        }
        if let Some(ended) = stopped_outcome(stopped, limits) {
            return ended;
        }
        if let Some(outcome) = told.borrow_mut().outcome.take() {
            return Ok(name_the_memory_limit(outcome, limits));
        }
        let batch = std::mem::take(&mut told.borrow_mut().started);
        if batch.is_empty() {
            return Ok(ScriptOutcome::Failed {
                error: "the script is waiting on a promise that nothing will resolve".to_string(),
            });
        }

        let (numbers, calls): (Vec<u32>, Vec<ScriptToolCall>) = batch.into_iter().unzip();
        let answers = answer(calls);
        if Instant::now() >= deadline {
            stopped
                .compare_exchange(RUNNING, OUT_OF_TIME, Ordering::Relaxed, Ordering::Relaxed)
                .ok();
        }
        if let Some(ended) = stopped_outcome(stopped, limits) {
            return ended;
        }
        for (number, answer) in numbers.into_iter().zip(answers) {
            let (ok, text) = match answer {
                Ok(text) => (true, text),
                Err(text) => (false, text),
            };
            if deliver.call::<_, JsValue>((number, ok, text)).is_err() {
                return ended_early(ctx, stopped, limits);
            }
        }
    }
}

/// The script was stopped: say why.
fn stopped_outcome(
    stopped: &AtomicU8,
    limits: ScriptEngineLimits,
) -> Option<io::Result<ScriptOutcome>> {
    match stopped.load(Ordering::Relaxed) {
        CANCELLED => Some(Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "the script was cancelled",
        ))),
        OUT_OF_TIME => Some(Ok(ScriptOutcome::Failed {
            error: format!(
                "the script ran longer than {} and was stopped",
                seconds(limits.wall_clock)
            ),
        })),
        _ => None,
    }
}

/// A call into the engine's own functions failed. Either the script was
/// stopped while it ran, or the script changed what those functions run on.
fn ended_early(
    ctx: &Ctx<'_>,
    stopped: &AtomicU8,
    limits: ScriptEngineLimits,
) -> io::Result<ScriptOutcome> {
    let _ = ctx.catch();
    match stopped_outcome(stopped, limits) {
        Some(ended) => ended,
        None => Ok(ScriptOutcome::Failed {
            error: "The script broke the functions it runs on".to_string(),
        }),
    }
}

/// QuickJS reports a refused allocation as an ordinary error. Say which limit
/// it was.
fn name_the_memory_limit(outcome: ScriptOutcome, limits: ScriptEngineLimits) -> ScriptOutcome {
    match outcome {
        ScriptOutcome::Failed { error } if error.contains("out of memory") => {
            ScriptOutcome::Failed {
                error: format!(
                    "the script used more than {} MiB",
                    limits.memory_bytes / (1024 * 1024)
                ),
            }
        }
        other => other,
    }
}

fn engine_error(error: rquickjs::Error) -> io::Error {
    io::Error::other(format!("the script engine could not start: {error}"))
}

fn seconds(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}
```

In `crates/orca-runtime/src/script/mod.rs`, add the module next to the existing two:

```rust
pub(crate) mod calls;
pub mod engine;
pub mod host;
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --test script_engine_contract --locked`
Expected: `test result: ok. 25 passed; 0 failed`

If `a_script_over_its_memory_limit_fails_and_names_the_limit` fails with a completed script, a custom allocator feature has been enabled on `rquickjs`; remove it.

- [ ] **Step 8: Lint**

Run: `cargo fmt --all && cargo clippy -p orca-runtime --all-targets --locked -j 1`
Expected: no warnings from `script/engine.rs` or `tests/script_engine_contract.rs`.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock crates/orca-runtime/Cargo.toml \
  crates/orca-runtime/src/script/engine.rs crates/orca-runtime/src/script/prelude.js \
  crates/orca-runtime/src/script/mod.rs tests/script_engine_contract.rs
git commit -m "feat(runtime): script engine on an embedded QuickJS"
```

---

### Task 2: The memory setting

**Files:**
- Modify: `crates/orca-core/src/config/mod.rs:317-338` (`ScriptToolConfig`)
- Test: `crates/orca-core/src/config/file.rs:2387-2422`

**Interfaces:**
- Consumes: nothing.
- Produces: `ScriptToolConfig.memory_limit_mb: u64` (default 64), `ScriptToolConfig::MIN_MEMORY_LIMIT_MB: u64 = 8`, `ScriptToolConfig::memory_limit_bytes(&self) -> usize`.

- [ ] **Step 1: Write the failing tests**

In `crates/orca-core/src/config/file.rs`, replace the struct literal in `script_tool_is_off_with_its_documented_limits_by_default` so it names the new field:

```rust
        assert_eq!(
            ToolConfig::default().script,
            crate::config::ScriptToolConfig {
                enabled: false,
                timeout_secs: 60,
                max_tool_calls: 100,
                memory_limit_mb: 64,
            }
        );
```

and add these two tests after `tools_script_table_is_read_from_the_config_file`:

```rust
    #[test]
    fn a_scripts_memory_limit_is_read_from_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        std::fs::write(&user_path, "[tools.script]\nmemory_limit_mb = 16\n").unwrap();

        let script = load_layered_config_from_paths(&user_path, &project_dir)
            .tools
            .script;

        assert_eq!(script.memory_limit_mb, 16);
        assert_eq!(script.memory_limit_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn a_scripts_memory_limit_is_never_under_the_least_the_engine_needs() {
        let script = crate::config::ScriptToolConfig {
            memory_limit_mb: 0,
            ..crate::config::ScriptToolConfig::default()
        };

        assert_eq!(script.memory_limit_bytes(), 8 * 1024 * 1024);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p orca-core --lib --locked script`
Expected: FAIL to compile with `struct ScriptToolConfig has no field named memory_limit_mb`.

- [ ] **Step 3: Add the setting**

In `crates/orca-core/src/config/mod.rs`, replace `ScriptToolConfig` and its `Default`:

```rust
pub struct ScriptToolConfig {
    /// Whether `run_script` is offered to the model and may run.
    pub enabled: bool,
    /// How long one script may run, in seconds, before it is stopped.
    pub timeout_secs: u64,
    /// How many tool calls one script may make.
    pub max_tool_calls: u32,
    /// How much memory one script may hold, in MiB.
    pub memory_limit_mb: u64,
}

impl ScriptToolConfig {
    /// The least a script is given whatever the setting says: under this the
    /// engine cannot hold its own functions and one tool result.
    pub const MIN_MEMORY_LIMIT_MB: u64 = 8;

    /// The memory limit in bytes, never under the least the engine needs.
    pub fn memory_limit_bytes(&self) -> usize {
        let mib = self.memory_limit_mb.max(Self::MIN_MEMORY_LIMIT_MB);
        usize::try_from(mib.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
    }
}

impl Default for ScriptToolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: 60,
            max_tool_calls: 100,
            memory_limit_mb: 64,
        }
    }
}
```

Keep the doc comment and the `#[derive(...)]` and `#[serde(default)]` lines above the struct as they are.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p orca-core --lib --locked script`
Expected: PASS, including the two new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-core/src/config/mod.rs crates/orca-core/src/config/file.rs
git commit -m "feat(config): memory limit for run_script"
```

---

### Task 3: Batches, and `run_script` on the engine

**Files:**
- Modify: `crates/orca-runtime/src/script/calls.rs` (all of the non-test code; the tests as listed)
- Modify: `crates/orca-runtime/src/script/mod.rs` (all of it)
- Modify: `crates/orca-runtime/src/tool_turn.rs:3069-3180` (four early returns)
- Modify: `crates/orca-tools/src/registry.rs:743` (one sentence of the description)

**Interfaces:**
- Consumes: Task 1's `ScriptEngine::run`, `ScriptEngineLimits`, `ScriptToolCall`, `ScriptOutcome`; Task 2's `memory_limit_bytes()`.
- Produces:
  - `pub(crate) trait ScriptToolExecutor: Sync { fn execute(&self, request: ToolRequest) -> ToolResult; }`
  - `ScriptToolCalls::new(script_call_id, cwd, mcp_registry, hooks, cancel, executor, max_calls: u32, max_parallel: usize)`
  - `ScriptToolCalls::answer(&mut self, calls: Vec<ScriptToolCall>) -> Vec<Result<String, String>>`
  - `ScriptToolCalls::made()` and `drain_report()` unchanged.

After this task `run_script` no longer starts a process. `host.rs` still compiles and is unused; Task 4 deletes it.

- [ ] **Step 1: Make the existing `calls.rs` tests speak in batches, and add the new ones**

In the `tests` module of `crates/orca-runtime/src/script/calls.rs`:

Replace the imports at the top of the module with:

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    use orca_core::hook_types::HookConfig;
    use serde_json::json;

    use super::*;
```

Replace `Recorder` and its two impls (it is shared between threads now, so the `RefCell` becomes a `Mutex`):

```rust
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
```

Replace `call` (a call no longer carries an id of its own) and add `one`:

```rust
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
```

Replace `Fixture::calls` so it takes any executor and a parallel limit:

```rust
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
```

Then, in the existing tests of the module:

- every `calls.answer(call(ARGS))` becomes `one(&mut calls, call(ARGS))`;
- every `executor.requests.borrow()` becomes `executor.requests()`;
- in `a_result_cut_short_is_an_error_not_a_partial_answer`, build `calls` with `fixture.calls(&CutShort, 10)` instead of the hand-written `ScriptToolCalls::new(...)`.

Add these tests at the end of the module:

```rust
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
            std::thread::sleep(std::time::Duration::from_millis(60u64.saturating_sub(number * 10)));
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
        assert!(answers[1].as_ref().unwrap_err().contains("is not available to scripts"));
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
        assert_eq!(calls.drain_report().len(), 2, "a call over the limit is not reported");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p orca-runtime --lib --locked script::calls`
Expected: FAIL to compile (`ScriptToolCall` has a `call_id` field the tests no longer give; `ScriptToolCalls::new` takes 7 arguments).

- [ ] **Step 3: Rewrite the non-test code of `calls.rs`**

Replace everything in `crates/orca-runtime/src/script/calls.rs` above `#[cfg(test)]` with:

```rust
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
```

A tool that panics on a worker thread makes `thread::scope` panic in turn; the engine's entry catches that and the script fails with `run_script failed inside the script engine`.

- [ ] **Step 4: Rewrite `mod.rs` to run scripts on the engine**

Replace all of `crates/orca-runtime/src/script/mod.rs` with:

```rust
//! `run_script`: JavaScript the model writes that calls read-only tools
//! itself, so one model round does the work of several.
//!
//! The script runs inside Orca, in a QuickJS created for it and dropped after
//! it ([`engine`]). It has no access of its own: each tool call it makes comes
//! to the runtime, which decides whether to run it and runs it the way it runs
//! a read the model asked for ([`calls`]). Only what the script returns goes
//! back to the model.

pub(crate) mod calls;
pub mod engine;
pub mod host;

use std::io;
use std::path::Path;
use std::sync::{Arc, OnceLock};
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
use crate::workflow::host::WorkflowHost;

/// Whether this machine has a Node. Scripts no longer need one; this is still
/// what decides whether the tool is offered until that is taken out too.
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
    let script = match serde_json::from_str::<RunScriptArguments>(
        request.raw_arguments.as_deref().unwrap_or("{}"),
    ) {
        Ok(arguments) if !arguments.script.trim().is_empty() => arguments.script,
        _ => return ToolResult::invalid_input(request, "run_script needs a non-empty `script`"),
    };

    let wall_clock = Duration::from_secs(settings.timeout_secs.max(1));
    let executor = ReadOnlyToolExecutor {
        cwd,
        mcp_registry,
        cancel,
        // The script's time is up inside a tool call as much as outside one.
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
            || self.cancel.is_cancelled() || Instant::now() >= self.deadline,
        )
    }
}
```

If the compiler says `ReadOnlyToolExecutor` is not `Sync`, the type it names is the one to look at: `McpRegistry` is an `Arc` of shared state that the read-only batch already sends to worker threads, and `CancelToken` is an `Arc<AtomicBool>`. Do not add `unsafe impl Sync`; report the type instead.

- [ ] **Step 5: Let the end-to-end tests run without Node**

In `crates/orca-runtime/src/tool_turn.rs`, delete these three lines from each of `run_script_reads_a_real_file_and_returns_only_what_the_script_computed`, `a_script_cannot_write_or_read_outside_the_workspace`, `a_long_script_result_is_cut_like_any_tool_output` and `a_script_stops_at_its_tool_call_limit_and_can_still_answer`:

```rust
        if !crate::script::node_available() {
            return;
        }
```

- [ ] **Step 6: Tell the model what it gained**

In `crates/orca-tools/src/registry.rs`, in the `run_script` description, after the sentence ending `(read a large file in parts with `offset` and `limit`).` insert:

```text
 Calls started together, as with `Promise.all`, run at the same time.
```

so the description reads `...(read a large file in parts with `offset` and `limit`). Calls started together, as with `Promise.all`, run at the same time. `console.log(...)` adds a line...`.

- [ ] **Step 7: Run the tests to verify they pass**

Run:

```bash
cargo test -p orca-runtime --lib --locked script::
cargo test -p orca-runtime --lib --locked run_script
cargo test -p orca-runtime --lib --locked a_script_
cargo test -p orca-runtime --lib --locked a_long_script_result
cargo test -p orca-tools --lib --locked run_script
```

Expected: all PASS. The `script::calls` tests include the five new ones; the `tool_turn` tests now run on a machine without Node.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all && cargo clippy -p orca-runtime -p orca-tools --all-targets --locked -j 1
git add crates/orca-runtime/src/script/calls.rs crates/orca-runtime/src/script/mod.rs \
  crates/orca-runtime/src/tool_turn.rs crates/orca-tools/src/registry.rs
git commit -m "feat(runtime): run_script runs in process and its calls run together"
```

---

### Task 4: Take out the Node host

**Files:**
- Delete: `crates/orca-runtime/src/script/host.rs`, `crates/orca-runtime/src/script/host.mjs`, `tests/script_host_contract.rs`
- Modify: `crates/orca-runtime/src/script/mod.rs`
- Modify: `crates/orca-runtime/src/tool_invocation.rs:201-208`, `:248-255`, `:772-800`
- Modify: `crates/orca-runtime/src/workflow/host.rs:573-1001` (visibility of six items)
- Modify: `crates/orca-tools/src/registry.rs:739-763`, `:3195-3196` (comments)
- Modify: `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json:421-427`
- Modify: `site/src/docs/md/en/configuration.mdx:250-266`, `site/src/docs/md/zh/configuration.mdx:225-241`

**Interfaces:**
- Consumes: Task 3 (nothing uses `script::host` any more).
- Produces: `omit_unavailable_session_tools(tools: &mut Vec<ProviderToolDefinition>, config: &RunConfig, workflow_child: bool)`. `script::node_available` and `script::host` no longer exist.

- [ ] **Step 1: Rewrite the advertisement test**

In `crates/orca-runtime/src/tool_invocation.rs`, replace `run_script_is_advertised_only_when_enabled_and_node_is_there` with:

```rust
    #[test]
    fn run_script_is_advertised_exactly_when_it_is_enabled() {
        for enabled in [true, false] {
            let mut config = config_with_external(Vec::new());
            config.tools.script.enabled = enabled;
            let mut tools = provider_tool_schema_override(
                0,
                &SubagentType::General,
                AgentToolPolicyContext::unrestricted(),
                &McpRegistry::default(),
                &[],
            )
            .expect("tool definitions");

            super::omit_unavailable_session_tools(&mut tools, &config, false);

            assert_eq!(
                tools.iter().any(|tool| tool.name == "run_script"),
                enabled,
                "enabled={enabled}"
            );
            assert!(tools.iter().any(|tool| tool.name == "read_file"));
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p orca-runtime --lib --locked run_script_is_advertised`
Expected: FAIL to compile (`this function takes 4 arguments but 3 arguments were supplied`).

- [ ] **Step 3: Remove the Node condition from advertisement**

In `crates/orca-runtime/src/tool_invocation.rs`, the call becomes:

```rust
    if let Some(tools) = tools_override.as_mut() {
        omit_unavailable_session_tools(tools, config, tool_policy.workflow_child);
```

and the function:

```rust
/// Only the advertised schema changes; hidden tools remain registered and executable.
pub(crate) fn omit_unavailable_session_tools(
    tools: &mut Vec<ProviderToolDefinition>,
    config: &RunConfig,
    workflow_child: bool,
) {
    tools.retain(|tool| match tool.name.as_str() {
        "run_script" => config.tools.script.enabled,
```

(the rest of the `match` is unchanged).

- [ ] **Step 4: Delete the host**

```bash
git rm crates/orca-runtime/src/script/host.rs crates/orca-runtime/src/script/host.mjs tests/script_host_contract.rs
```

In `crates/orca-runtime/src/script/mod.rs`:

- delete the line `pub mod host;`;
- delete the `node_available` function and its doc comment;
- change `use std::sync::{Arc, OnceLock};` to `use std::sync::Arc;`;
- delete `use crate::workflow::host::WorkflowHost;`.

- [ ] **Step 5: Close what was opened for the host**

In `crates/orca-runtime/src/workflow/host.rs`, change `pub(crate)` back to private on each of these, which only the script host used from outside the file:

```text
pub(crate) struct WorkflowHostFileGuard      -> struct WorkflowHostFileGuard
    pub(crate) fn new(path: PathBuf)         ->     fn new(path: PathBuf)
pub(crate) struct WorkflowHostChild          -> struct WorkflowHostChild
    pub(crate) fn new(child: Child, ...)     ->     fn new(child: Child, ...)
    pub(crate) fn child_mut(&mut self)       ->     fn child_mut(&mut self)
    pub(crate) fn force_terminate_and_wait   ->     fn force_terminate_and_wait
pub(crate) fn read_bounded_workflow_frame    -> fn read_bounded_workflow_frame
pub(crate) fn spawn_retained_reader          -> fn spawn_retained_reader
pub(crate) fn node_entry_path                -> fn node_entry_path
pub(crate) fn node_confinement_args          -> fn node_confinement_args
```

Leave `pub(crate) fn run_with_agent_event_control_and_ipc_paths` alone: it was not opened for the script host. If the compiler reports that one of the ten is used from another file, leave that one `pub(crate)`.

- [ ] **Step 6: Correct the comments that describe a process**

In `crates/orca-tools/src/registry.rs`:

```rust
        // A script may run for a while: an interrupt stops it rather than
        // waiting it out.
        cooperative_builtin_spec(
            "run_script",
```

```rust
            // It runs its own calls and their hooks: never one of a read batch.
            false,
```

and in the test `run_script_is_a_read_only_tool_the_read_batch_never_takes`:

```rust
        // It runs calls and hooks of its own, so it must not be dispatched
        // as one of a batch of plain reads.
        assert!(!resolved.spec.concurrent_safe);
```

- [ ] **Step 7: Remove the host's row from the Windows manifest**

In `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json`, delete this row and the comma that separates it from its neighbour:

```json
    [
      "script-host-unix-process-api",
      "crates/orca-runtime/src/script/host.rs",
      "unix_std_api",
      1,
      "process-ownership-plan"
    ],
```

- [ ] **Step 8: Update the documentation**

In `site/src/docs/md/en/configuration.mdx`, replace the code block and the first bullet of "Scripts that call tools (experimental)":

````markdown
```toml
[tools.script]
enabled = true
timeout_secs = 60      # how long one script may run
max_tool_calls = 100   # how many tool calls one script may make
memory_limit_mb = 64   # how much memory one script may hold
```
````

```markdown
- The script runs inside Orca, in a JavaScript engine that has no file, network or process access of its own; every read goes through Orca's own tools. Nothing needs to be installed.
- Calls a script starts together, as with `Promise.all`, run at the same time, up to `max_read_parallel` at once.
```

In `site/src/docs/md/zh/configuration.mdx`, the same two replacements:

````markdown
```toml
[tools.script]
enabled = true
timeout_secs = 60      # 一个脚本最多运行多久
max_tool_calls = 100   # 一个脚本最多调用多少次工具
memory_limit_mb = 64   # 一个脚本最多占用多少内存
```
````

```markdown
- 脚本在 Orca 进程内的 JavaScript 引擎里运行，引擎自身没有文件、网络和进程访问能力；所有读取都经过 Orca 自己的工具。不需要安装任何东西。
- 脚本同时发起的调用（比如用 `Promise.all`）会并发执行，同时最多 `max_read_parallel` 个。
```

- [ ] **Step 9: Verify**

Run:

```bash
cargo build --workspace --all-targets --locked
cargo test -p orca-runtime --lib --locked run_script_is_advertised
cargo test --test script_engine_contract --locked
cargo test --test workflow_host_contract --locked
grep -rn "node_available\|script::host\|ScriptHost\|host\.mjs" crates/orca-runtime/src/script crates/orca-runtime/src/tool_invocation.rs crates/orca-runtime/src/tool_turn.rs tests/script_engine_contract.rs
node scripts/validate-windows-platform-boundaries.mjs
node scripts/validate-execution-broker-boundary.mjs
node scripts/validate-runtime-surface-contract.mjs
```

Expected: the build and the three test runs pass; the `grep` prints nothing; the three validators exit 0. If the Windows validator reports a total that no longer matches, change that total in the manifest by exactly the one row removed and run it again.

- [ ] **Step 10: Lint and commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --locked -j 1
git add -A crates/orca-runtime/src/script crates/orca-runtime/src/tool_invocation.rs \
  crates/orca-runtime/src/workflow/host.rs crates/orca-tools/src/registry.rs tests \
  docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json \
  site/src/docs/md/en/configuration.mdx site/src/docs/md/zh/configuration.mdx
git commit -m "refactor(runtime): remove the Node script host"
```

---

### Task 5: The comparison says whether an answer is right

**Files:**
- Create: `scripts/eval/code_mode/expected.py`
- Create: `scripts/eval/code_mode/test_expected.py`
- Modify: `scripts/eval/code_mode/run.py`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `expected.is_right(task_id: str, answer: str, root: Path) -> bool`; `run.py` rows gain `"right": bool`; `run.py` writes `summary.json` with `{"off": {...}, "on": {...}, "default_on": bool}`.

- [ ] **Step 1: Write the failing tests**

Create `scripts/eval/code_mode/test_expected.py`:

```python
"""Tests for expected.py.

Run with: python3 -m unittest scripts/eval/code_mode/test_expected.py
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import expected  # noqa: E402


def checkout(files: dict[str, str]) -> Path:
    root = Path(tempfile.mkdtemp(prefix="code-mode-expected-"))
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


class UserTrustedLaunches(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-runtime/src/hooks.rs": "broker.launch_user_trusted(a);",
            "crates/orca-runtime/src/workflow/host.rs":
                "x.launch_user_trusted(a); y.launch_user_trusted(b);",
            "crates/orca-runtime/src/quiet.rs": "fn nothing() {}",
        })

    def test_every_file_with_its_count_is_right(self):
        answer = "crates/orca-runtime/src/hooks.rs: 1\ncrates/orca-runtime/src/workflow/host.rs: 2"
        self.assertTrue(expected.is_right("user-trusted-launches", answer, self.root))

    def test_paths_under_the_directory_asked_about_are_accepted(self):
        answer = "| hooks.rs | 1 |\n| workflow/host.rs | 2 |"
        self.assertTrue(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_wrong_count_is_wrong(self):
        answer = "crates/orca-runtime/src/hooks.rs: 1\ncrates/orca-runtime/src/workflow/host.rs: 3"
        self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))

    def test_a_file_that_has_none_is_wrong(self):
        answer = "hooks.rs: 1\nworkflow/host.rs: 2\nquiet.rs: 1"
        self.assertFalse(expected.is_right("user-trusted-launches", answer, self.root))


class WorkflowEvents(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-core/src/event_schema.rs":
                'a("workflow.started"); b("workflow.phase.completed"); c("tool.started");',
        })

    def test_all_of_them_and_the_total_is_right(self):
        answer = "workflow.phase.completed\nworkflow.started\n\nTotal: 2."
        self.assertTrue(expected.is_right("workflow-events", answer, self.root))

    def test_one_missing_is_wrong(self):
        self.assertFalse(expected.is_right("workflow-events", "workflow.started\nTotal: 1", self.root))

    def test_a_wrong_total_is_wrong(self):
        answer = "workflow.phase.completed\nworkflow.started\nTotal: 3"
        self.assertFalse(expected.is_right("workflow-events", answer, self.root))


class NetworkTools(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-tools/src/registry.rs": """
                registry.register(BuiltinTool::new(builtin_spec(
                    "read_file", "Read.", json!({}), CapabilitySet::read_only_fs(),
                )));
                registry.register(BuiltinTool::new(cooperative_builtin_spec(
                    "web_search", "Search.", json!({}),
                    CapabilitySet::network_search(),
                )));
                fn of(kind: ActionKind) { ActionKind::Network => CapabilitySet::network_search() }
            """,
        })

    def test_the_network_tools_alone_are_right(self):
        self.assertTrue(expected.is_right("network-tools", "`web_search`", self.root))

    def test_another_tool_named_with_them_is_wrong(self):
        self.assertFalse(expected.is_right("network-tools", "web_search, read_file", self.root))


class LargestTestFiles(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            f"tests/t{count}.rs": "#[test]\nfn x() {}\n" * count for count in (9, 7, 5, 4, 3, 1)
        })

    def test_the_five_largest_in_order_with_counts_is_right(self):
        answer = "\n".join(f"tests/t{count}.rs — {count}" for count in (9, 7, 5, 4, 3))
        self.assertTrue(expected.is_right("largest-test-files", answer, self.root))

    def test_out_of_order_is_wrong(self):
        answer = "\n".join(f"tests/t{count}.rs — {count}" for count in (7, 9, 5, 4, 3))
        self.assertFalse(expected.is_right("largest-test-files", answer, self.root))

    def test_a_count_cut_short_is_wrong(self):
        answer = "\n".join(f"tests/t{count}.rs — {count}" for count in (9, 7, 5, 4, 2))
        self.assertFalse(expected.is_right("largest-test-files", answer, self.root))


class CredentialScrubSites(unittest.TestCase):
    def setUp(self):
        self.root = checkout({
            "crates/orca-platform/src/process.rs":
                "pub fn remove_credential_env(c: &mut Command) {}\nfn t() { remove_credential_env(&mut c); }",
            "crates/orca-tools/src/bash.rs": "process::remove_credential_env(&mut command);",
            "crates/orca-tools/src/quiet.rs": "fn nothing() {}",
        })

    def test_every_calling_file_is_right_with_or_without_the_defining_one(self):
        self.assertTrue(expected.is_right(
            "credential-scrub-sites", "crates/orca-tools/src/bash.rs: the shell", self.root))
        self.assertTrue(expected.is_right(
            "credential-scrub-sites",
            "crates/orca-tools/src/bash.rs: the shell\ncrates/orca-platform/src/process.rs: a test",
            self.root))

    def test_a_missing_file_is_wrong(self):
        self.assertFalse(expected.is_right(
            "credential-scrub-sites", "crates/orca-platform/src/process.rs: a test", self.root))

    def test_a_file_that_does_not_call_it_is_wrong(self):
        self.assertFalse(expected.is_right(
            "credential-scrub-sites",
            "crates/orca-tools/src/bash.rs: the shell\ncrates/orca-tools/src/quiet.rs: something",
            self.root))


class UnknownTask(unittest.TestCase):
    def test_a_task_with_no_checker_is_an_error(self):
        with self.assertRaises(KeyError):
            expected.is_right("no-such-task", "anything", Path("."))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python3 -m unittest scripts/eval/code_mode/test_expected.py`
Expected: FAIL with `ModuleNotFoundError: No module named 'expected'`.

- [ ] **Step 3: Write the checker**

Create `scripts/eval/code_mode/expected.py`:

```python
"""The right answer to each task in tasks.json, and whether an answer gives it.

The right answer is read from the checkout the question was asked about, not
stored: the questions count things in the source, and the source changes.
"""

from __future__ import annotations

import re
from pathlib import Path

RUNTIME_SRC = "crates/orca-runtime/src/"
RUST_PATH = re.compile(r"[\w./-]*\w\.rs")
NUMBER = re.compile(r"(?<![\w.])\d+(?!\w|\.\d)")


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def _numbers(text: str) -> set[str]:
    return set(NUMBER.findall(text))


def _names(path: str, mentioned: str) -> bool:
    """Whether `mentioned`, a path as an answer wrote it, is `path`."""
    return path == mentioned or path.endswith("/" + mentioned)


def _only_these_files(answer: str, allowed: list[str]) -> bool:
    """Every Rust file the answer names is one of `allowed`."""
    return all(any(_names(path, mentioned) for path in allowed)
               for mentioned in RUST_PATH.findall(answer))


def _each_with_its_count(answer: str, counts: dict[str, int], under: str = "") -> bool:
    """Every path is on a line of the answer that also gives its count."""
    for path, count in counts.items():
        short = path.removeprefix(under)
        if not any(short in line and str(count) in _numbers(line.replace(short, " "))
                   for line in answer.splitlines()):
            return False
    return True


def _user_trusted_launches(answer: str, root: Path) -> bool:
    counts = {}
    for path in sorted((root / RUNTIME_SRC).rglob("*.rs")):
        count = _read(path).count("launch_user_trusted(")
        if count:
            counts[path.relative_to(root).as_posix()] = count
    return (_each_with_its_count(answer, counts, under=RUNTIME_SRC)
            and _only_these_files(answer, list(counts)))


def _workflow_events(answer: str, root: Path) -> bool:
    source = _read(root / "crates/orca-core/src/event_schema.rs")
    events = set(re.findall(r'"(workflow\.[a-z_.]+)"', source))
    named = {name.rstrip(".") for name in re.findall(r"workflow\.[a-z_.]+", answer)}
    return named == events and str(len(events)) in _numbers(answer)


def _network_tools(answer: str, root: Path) -> bool:
    source = _read(root / "crates/orca-tools/src/registry.rs")
    every = set(re.findall(r'_spec\(\s*"([a-z_]+)",', source))
    network = set(re.findall(
        r'_spec\(\s*"([a-z_]+)",(?:(?!_spec\().)*?CapabilitySet::network_search\(\)',
        source, re.S))
    named = set(re.findall(r"[a-z_]+", answer)) & every
    return bool(network) and named == network


def _largest_test_files(answer: str, root: Path) -> bool:
    counts = {path.relative_to(root).as_posix(): _read(path).count("#[test]")
              for path in (root / "tests").glob("*.rs")}
    largest = sorted(counts, key=lambda path: (-counts[path], path))[:5]
    if not _each_with_its_count(answer, {path: counts[path] for path in largest}):
        return False
    first_seen = [answer.find(path) for path in largest]
    return first_seen == sorted(first_seen)


def _credential_scrub_sites(answer: str, root: Path) -> bool:
    calling, defining = [], []
    for path in sorted((root / "crates").rglob("*.rs")):
        source = _read(path)
        name = path.relative_to(root).as_posix()
        if "fn remove_credential_env(" in source:
            defining.append(name)
        elif "remove_credential_env(" in source:
            calling.append(name)
    return (all(path in answer for path in calling)
            and _only_these_files(answer, calling + defining))


CHECKS = {
    "user-trusted-launches": _user_trusted_launches,
    "workflow-events": _workflow_events,
    "network-tools": _network_tools,
    "largest-test-files": _largest_test_files,
    "credential-scrub-sites": _credential_scrub_sites,
}


def is_right(task_id: str, answer: str, root: Path) -> bool:
    """Whether `answer` is the right answer to `task_id` for the checkout at `root`."""
    return CHECKS[task_id](answer, Path(root))
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python3 -m unittest scripts/eval/code_mode/test_expected.py`
Expected: `Ran 16 tests` ... `OK`.

- [ ] **Step 5: Mark each run and sum the comparison**

In `scripts/eval/code_mode/run.py`:

Change the module docstring's sentence `It measures cost, not correctness: read the answers.` to:

```text
Each answer is marked right or wrong against the checkout (expected.py), and
summary.json says whether the totals meet the rule for turning `run_script`
on by default.
```

Add after `HERE = ...`:

```python
import sys

sys.path.insert(0, str(HERE))

import expected  # noqa: E402
```

Replace `make_home` so both variants say what they are (the default may change, and "off" must stay off):

```python
def make_home(source: Path, scripts_on: bool) -> Path:
    home = Path(tempfile.mkdtemp(prefix="orca-code-mode-eval-"))
    home.chmod(0o700)
    for name in ("auth.json", "config.toml"):
        if (source / name).exists():
            shutil.copy2(source / name, home / name)
    with open(home / "config.toml", "a", encoding="utf-8") as config:
        config.write(f"\n[tools.script]\nenabled = {'true' if scripts_on else 'false'}\n")
    return home
```

Add before `main`:

```python
def compare(rows: list[dict]) -> dict:
    """Totals for each variant, and whether they meet the rule for default-on:
    at least as many right answers, fewer model rounds, lower cost."""
    totals = {}
    for variant in ("off", "on"):
        mine = [row for row in rows if row["variant"] == variant]
        totals[variant] = {
            "runs": len(mine),
            "right": sum(1 for row in mine if row["right"]),
            "rounds": sum(row["rounds"] for row in mine),
            "cost_usd": round(sum(row["cost_usd"] or 0 for row in mine), 6),
        }
    off, on = totals["off"], totals["on"]
    totals["default_on"] = (on["runs"] > 0 and on["right"] >= off["right"]
                            and on["rounds"] < off["rounds"] and on["cost_usd"] < off["cost_usd"])
    return totals
```

In `main`, build each row with its mark:

```python
                    row = {"task": task["id"], "variant": variant, "repetition": repetition,
                           **result, **summarize((out / f"{name}.jsonl").read_text(encoding="utf-8"))}
                    row["right"] = expected.is_right(task["id"], row["answer"], Path(args.cwd))
                    rows.append(row)
```

and after `results.json` is written:

```python
    summary = compare(rows)
    (out / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary), flush=True)
```

- [ ] **Step 6: Check the runner still parses**

Run: `python3 scripts/eval/code_mode/run.py --help && python3 -m unittest scripts/eval/code_mode/test_expected.py`
Expected: the usage text, then `OK`.

- [ ] **Step 7: Commit**

```bash
git add scripts/eval/code_mode/expected.py scripts/eval/code_mode/test_expected.py scripts/eval/code_mode/run.py
git commit -m "test(eval): mark run_script comparison answers right or wrong"
```

---

### Task 6: Measure, decide the default, and close

**Files:**
- Modify: `docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md` (add "Results")
- Modify only if the rule is met: `crates/orca-core/src/config/mod.rs` (`ScriptToolConfig::default`), `crates/orca-core/src/config/file.rs` (one test), `site/src/docs/md/en/configuration.mdx`, `site/src/docs/md/zh/configuration.mdx`

**Interfaces:**
- Consumes: Tasks 1 to 5.
- Produces: the measured table in the spec, and the default it decides.

The comparison spends provider credit: 30 `orca exec` runs with the key in `~/.orca/auth.json`. `run.py` copies the key into throwaway homes and does not change the user's files.

- [ ] **Step 1: Build what is being measured**

```bash
cargo build --release --locked
stat -f%z target/release/orca > /tmp/orca-size-after.txt   # Linux: stat -c%s
echo "before $(cat /tmp/orca-size-before.txt) after $(cat /tmp/orca-size-after.txt)"
```

- [ ] **Step 2: Run one task once, to see the runner works end to end**

```bash
python3 scripts/eval/code_mode/run.py --binary target/release/orca --cwd "$PWD" \
  --out /tmp/code-mode-eval-probe --only workflow-events --repeat 1
```

Expected: two JSON lines (off, on) each with `"exit": 0` and a `"right"` field, then the summary line. If `exit` is not 0, read `/tmp/code-mode-eval-probe/workflow-events.on.0.jsonl` before going on.

- [ ] **Step 3: Run the comparison**

```bash
python3 scripts/eval/code_mode/run.py --binary target/release/orca --cwd "$PWD" \
  --out /tmp/code-mode-eval --repeat 3
cat /tmp/code-mode-eval/summary.json
```

Expected: 30 row lines and a summary with `"runs": 15` for each variant.

- [ ] **Step 4: Read the wrong answers**

For every row of `/tmp/code-mode-eval/results.json` with `"right": false`, read its `answer`. A right answer the checker marked wrong (a format it does not recognise) is a checker defect: fix `expected.py`, add the answer's shape to `test_expected.py`, and recompute the marks from the saved answers without running the model again:

```bash
python3 - <<'EOF'
import json, sys
from pathlib import Path
sys.path.insert(0, "scripts/eval/code_mode")
import expected, run
out = Path("/tmp/code-mode-eval")
rows = json.loads((out / "results.json").read_text())
for row in rows:
    row["right"] = expected.is_right(row["task"], row["answer"], Path.cwd())
(out / "results.json").write_text(json.dumps(rows, indent=2, ensure_ascii=False))
summary = run.compare(rows)
(out / "summary.json").write_text(json.dumps(summary, indent=2))
print(json.dumps(summary))
EOF
```

- [ ] **Step 5: Record the results in the spec**

Append to `docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md`, with the numbers from `summary.json` and the two size files in place of each `<...>`:

```markdown
## Results

Measured on <date> at `<commit>`, five tasks, three runs each way, through
`orca exec` against the real provider.

| | `run_script` off | `run_script` on |
|---|---|---|
| Runs | <off.runs> | <on.runs> |
| Answers right | <off.right> | <on.right> |
| Model rounds | <off.rounds> | <on.rounds> |
| Cost (USD) | <off.cost_usd> | <on.cost_usd> |

Release binary on `aarch64-apple-darwin`: <before> bytes before, <after> bytes
after.

The rule for default-on (as many right answers, fewer rounds, lower cost) is
<met | not met>: `enabled` is <`true` | `false`> by default.
```

These `<...>` are filled from files produced by Steps 1 and 3; leaving any of them in the committed spec is a defect.

- [ ] **Step 6: If `default_on` is `true`, turn the tool on by default**

Skip this step when `summary.json` says `"default_on": false`.

In `crates/orca-core/src/config/mod.rs`, in `impl Default for ScriptToolConfig`, set `enabled: true`, and change the struct's doc comment line `Off unless turned on.` to `On unless turned off.`.

In `crates/orca-core/src/config/file.rs`, rename `script_tool_is_off_with_its_documented_limits_by_default` to `script_tool_is_on_with_its_documented_limits_by_default`, expect `"script = true"` from `format_config_show`, and `enabled: true` in the struct literal.

In `site/src/docs/md/en/configuration.mdx`, change the section title to `## Scripts that call tools`, the sentence `It is off by default:` to `It is on by default; this turns it off, and shows the limits you can change:`, and `enabled = true` in the code block to `enabled = false`. Change the last line of the section to `The settings are read from the user configuration; a project's `.orca/config.toml` cannot change them.`

In `site/src/docs/md/zh/configuration.mdx`, the same: title `## 让脚本调用工具`, `默认关闭：` to `默认开启；下面的配置可以关掉它，也列出了可以调整的上限：`, `enabled = true` to `enabled = false`, and the last line to `这些设置只从用户配置读取；项目的 `.orca/config.toml` 不能修改它们。`

Then run the whole suite, because any test that lists the tools a default session is offered now has one more:

```bash
cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0
```

For each failure, read the assertion. If it compares a list of the tools offered by default, or the output of `orca config show`, add `run_script` (or `script = true`) to what it expects. Any other failure is not caused by the default: stop and report it, with the test's name and output, rather than changing the test.

- [ ] **Step 7: Run every gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 1
cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0
python3 -m unittest scripts/eval/code_mode/test_expected.py
node scripts/test-validate-runtime-surface-contract.mjs && node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-windows-platform-boundaries.mjs && node scripts/validate-windows-platform-boundaries.mjs
node scripts/test-validate-execution-broker-boundary.mjs && node scripts/validate-execution-broker-boundary.mjs
node scripts/test-validate-http-client-boundary.mjs && node scripts/validate-http-client-boundary.mjs
```

Expected: every command exits 0. A test that fails here and passes when run alone is to be reported by name, not retried until green.

- [ ] **Step 8: Commit**

```bash
git add -A docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md \
  crates/orca-core/src/config site/src/docs/md
git commit -m "feat(config): record the run_script comparison and set its default"
```

The branch is now ready to push. Pushing, the pull request and the release follow the project's release process and are not part of this plan. The release note of the version that ships this takes its numbers from the spec's "Results".
