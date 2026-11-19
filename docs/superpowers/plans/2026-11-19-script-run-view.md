# Seeing a Script Run in the TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `run_script` row in the conversation expands to the script the model wrote, each call it made (arguments, outcome, start of the result, which ran together) and what went back to the model; `/scripts` prints what the model is told about the tool.

**Architecture:** The runtime keeps writing a script's calls to the tool's output stream, but writes a summary line plus a short preview per call and marks the calls of one batch. The TUI keeps, per `run_script` row, the script and the calls text beside the result, composes them into the row's output text, and renders that text with one small function of its own. No surface type, event field or enum variant that existing code constructs is changed.

**Tech Stack:** Rust 2024 (`orca-runtime`, `orca-tui`, ratatui), Python 3 for the comparison runner.

**Spec:** `docs/superpowers/specs/2026-11-19-script-run-view-design.md`

## Global Constraints

- Summary line of one call, exactly: `{n:>3} {mark} {tool} {arguments} · {ok|failed} · {ms}ms[ · {size}]`. `mark` is a space for a batch of one; `┬` first, `├` between, `└` last for a batch of several. `size` only for `ok`.
- Arguments: compact JSON, cut at 200 characters with `…`.
- Preview: at most 5 lines, each cut at 120 characters with `…`, at most 400 bytes in all, each indented by ten spaces; `… {k} more line(s)` (same indent) when lines were left out; none for empty text.
- Size: `{bytes} B` under 1024, otherwise `{:.1} KiB`.
- Last line of the stream: `{made} tool call(s) · {ms}ms`.
- A preview line starts with at least six spaces; a summary line never does.
- A call past the call limit is not counted and not written.
- Section titles, exactly: `Script`, `Calls`, `Returned to the model · {size}`, `Failed`, and the line `Calls are shown only in the session that ran the script`.
- `ChatMessage::ToolCall`'s fields and `TuiEvent::ToolRequested`'s fields do not change (dozens of tests construct them).
- No surface type changes: the surface contract manifest and its hashes stay as they are.
- Every `cargo` command uses `--locked` and `CARGO_INCREMENTAL=0`.
- Commit after every task. Do not push.
- The code this plan quotes from `script/calls.rs` and `script/mod.rs` was read before the stage-1 fix wave changed those files (a `stopped()` method on the executor, a clamp on the wall clock, the engine on its own thread). Where a quoted "replace this with" no longer matches the file exactly, make the same change to the code as it now stands and say so in the report; do not revert anything the fix wave added.
- Existing failure texts of the engine do not change: `the script ran longer than {N}s and was stopped`, `the script used more than {N} MiB`, `the script is waiting on a promise that nothing will resolve`, `The script broke the functions it runs on`, `run_script failed inside the script engine`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/orca-runtime/src/script/calls.rs` | What is written for each call of a script |
| `crates/orca-runtime/src/script/mod.rs` | The stream's last line; what `/scripts` needs to know about the tool's limits |
| `crates/orca-tui/src/script_run.rs` (new) | One script run as shown: its parts, the text they compose, which lines are titles |
| `crates/orca-tui/src/protocol.rs` | `TuiEvent::ToolScript` |
| `crates/orca-tui/src/surface_projection.rs`, `hosted_session.rs` | Passing a `run_script` request's script on; rebuilding a run from a saved conversation |
| `crates/orca-tui/src/state_reducer.rs`, `transcript_state.rs` | Keeping a run's parts and the row's text in step |
| `crates/orca-tui/src/ui.rs` | Drawing a script run's row |
| `crates/orca-tui/src/script_prompt.rs` (new) | The text `/scripts` prints |
| `crates/orca-tui/src/commands/mod.rs`, `slash_command_actions.rs` | The `/scripts` command |

---

### Task 0: Close what the engine's final review left open

Carried from the stage-1 final review (`2026-11-19-script-quickjs-engine-design.md`), which allowed one fix wave. Three small things in the engine, unrelated to the view but on the same code.

**Files:**
- Modify: `crates/orca-runtime/src/script/engine.rs`
- Modify: `tests/script_engine_contract.rs`
- Modify: `docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md`
- Modify: `site/src/docs/md/en/configuration.mdx`, `site/src/docs/md/zh/configuration.mdx` (only if they state the argument limit)

**Interfaces:**
- Consumes: nothing.
- Produces: nothing other tasks use. `ScriptEngine::run`'s signature and every existing failure text stay as they are.

**What is wrong**

1. `start_call` (the native function behind `tools.x(...)`) caps one call's argument text at 1 MiB and the waiting calls at 1000, but parses each call's text into a `serde_json::Value` at once and keeps the parsed value until hand-over. A 1 MiB argument made of tiny objects parses to about 100 MB, so ten waiting calls hold a gigabyte outside the script's memory limit (measured).
2. The engine thread drops the context and the runtime before it tells the caller how the script ended. A script that finished within a few milliseconds of its deadline can be reported `the script ran longer than …` because the teardown crossed the deadline.
3. The design says the interrupt does not reach `indexOf` on an object whose `length` is `2 ** 53`. It does (`Array.prototype.indexOf` polls for interrupts); what needs the caller to stop it is `String.prototype.indexOf` over very long strings. And "Results" says each task can be answered in two or three rounds of `grep`; one task took 5 to 8.

**What to do**

- [ ] **Step 1: Write the failing tests** in `tests/script_engine_contract.rs`, in the file's existing style:
  - `the_arguments_waiting_to_be_handed_over_are_limited_in_all`: a script starts, without awaiting, calls whose arguments are each 100 KiB of text (`tools.read_file({ path: "x".repeat(100 * 1024) })`), and collects how each settles. The first two are handed to `answer`; the third rejects with an `Error` whose message is exactly `tool call arguments waiting to be answered are limited to 256 KiB in all; await some calls before starting more`; after `await`ing the first two, a further call of the same size is accepted. Assert on what `answer` was handed (two calls in the first batch, one later) and on the rejection text.
  - `one_call_over_the_limit_is_refused_by_itself`: a single call with 300 KiB of argument text rejects with the same message and `answer` is never called for it.
  - `what_a_waiting_call_holds_is_its_text`: a script starts one call whose argument is 200 KiB of text that parses to a very large value (`{ path: Array(50000).fill({ a: 0 }) }` is about 400 KiB — pick a count that stays under 256 KiB of text) and does not await it before a second, small call; both are handed over in one batch and the first call's `arguments` equals the value the script passed. (This pins that parsing still gives the caller the same `ScriptToolCall`.)
  - `a_script_that_finishes_just_inside_its_time_is_completed`: a script that holds about 40 MiB (for example an array of 40 strings of 1 MiB built with `"x".repeat`) and returns `"done"`, run with a wall clock generous enough that the script itself always finishes (5 s); assert `Completed`. This guards the ordering in Step 3; it cannot fail by timing, so also add a unit test beside the engine's own tests (`engine.rs`'s `tests` module) that asserts on the order of the engine thread's last two actions if they are separable (the outcome is sent, then the runtime is dropped) — if the code does not let that be observed without restructuring, say so in your report and keep only the contract test.

- [ ] **Step 2: Run them to verify they fail**: `CARGO_INCREMENTAL=0 cargo test --test script_engine_contract --locked the_arguments_waiting one_call_over what_a_waiting_call` — the first two FAIL (the old limits are 1 MiB per call and no total).

- [ ] **Step 3: Fix**
  - Waiting calls keep their arguments as text. `start_call` adds the text's byte length to a running total of what is waiting; a call that would take the total past `256 * 1024` bytes is refused with the message above and nothing of it is kept. The total falls back to zero when the waiting calls are handed over. The text is parsed into `serde_json::Value` when the batch is handed over, on the engine thread, one call at a time, as the batch is built. The 1 MiB per-call constant and its message (`a tool call's arguments are limited to 1 MiB`) are removed: the total subsumes it. The 1000-waiting-calls limit and its message stay.
  - The engine thread sends how the script ended before it drops the context and the runtime. Nothing of the script may run after that send: the values it holds are only dropped. Keep the existing guarantee that every JavaScript value is dropped before the runtime.

- [ ] **Step 4: Correct the documents**
  - In `docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md`: wherever the argument limit is described (the "Limits" table and the paragraph about the removed line limit), say: the calls a script has started and not yet been answered may hold at most 256 KiB of argument text in all, and at most 1000 calls may be waiting. Replace both statements about `indexOf` on an object whose `length` is `2 ** 53` with the true one: among the loops the interrupt does not reach are `join` and `reverse` over such an object and `String.prototype.indexOf` over very long strings. In "Results", replace "two or three rounds" with what the saved runs show: two to four rounds for four of the tasks and five to eight for the fifth.
  - If `site/src/docs/md/en/configuration.mdx` or the Chinese page mentions the 1 MiB argument limit, change it to the 256 KiB total in both; if neither mentions it, leave them.

- [ ] **Step 5: Verify**

```bash
CARGO_INCREMENTAL=0 cargo test --test script_engine_contract --locked
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked script::
cargo fmt --all && CARGO_INCREMENTAL=0 cargo clippy -p orca-runtime --all-targets --locked -j 1
```

Expected: all pass (the contract file had 43 tests; it now has those plus the new ones); run the contract file three times and report the three result lines. Any existing test that asserted the 1 MiB per-call message is rewritten to the new limit and message; name it in your report.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/script/engine.rs tests/script_engine_contract.rs \
  docs/superpowers/specs/2026-11-19-script-quickjs-engine-design.md site/src/docs/md
git commit -m "fix(runtime): bound what a script's waiting calls hold; say how a script ended before tearing it down"
```

---

### Task 1: What the runtime writes for each call

**Files:**
- Modify: `crates/orca-runtime/src/script/calls.rs`
- Modify: `crates/orca-runtime/src/script/mod.rs`
- Modify: `crates/orca-runtime/src/tool_turn.rs` (three assertions in the `run_script` tests)
- Modify: `scripts/eval/code_mode/run.py`, `scripts/eval/code_mode/test_run.py`

**Interfaces:**
- Consumes: nothing.
- Produces: the stream format of the Global Constraints; `pub fn orca_runtime::script::shown_size(bytes: usize) -> String`; `pub fn orca_runtime::script::script_tools() -> &'static [&'static str]`; `pub fn orca_runtime::script::script_result_limit_bytes() -> usize`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `crates/orca-runtime/src/script/calls.rs`, add this helper and these tests, and replace the body of `each_call_is_reported_once_with_how_it_ended` and the `reported` part of `answers_and_report_follow_the_order_the_calls_were_started_in` as shown.

```rust
    /// The report's summary lines, each cut after `ok` or `failed`.
    fn summaries(report: &[String]) -> Vec<String> {
        report
            .iter()
            .filter(|line| !line.starts_with("      "))
            .map(|line| {
                let end = line
                    .find(" · ok")
                    .map(|at| at + " · ok".len())
                    .or_else(|| line.find(" · failed").map(|at| at + " · failed".len()))
                    .expect("a summary line says how the call ended");
                line[..end].to_string()
            })
            .collect()
    }

    /// Answers every request with the text it was built with.
    struct Says(&'static str);

    impl ScriptToolExecutor for Says {
        fn execute(&self, request: ToolRequest) -> ToolResult {
            ToolResult::completed(&request, self.0.to_string(), false)
        }
    }

    #[test]
    fn each_call_is_reported_once_with_how_it_ended() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let _ = one(&mut calls, call("read_file", json!({ "path": "src/a.rs" })));
        let _ = one(&mut calls, call("grep", json!({ "pattern": "TODO", "path": "src" })));
        let _ = one(&mut calls, call("git_status", json!({})));
        let _ = one(&mut calls, call("bash", json!({ "command": "true" })));
        let report = calls.drain_report();

        assert_eq!(
            summaries(&report),
            [
                r#"  1   read_file {"path":"src/a.rs"} · ok"#,
                r#"  2   grep {"path":"src","pattern":"TODO"} · ok"#,
                r#"  3   git_status {} · ok"#,
                r#"  4   bash {"command":"true"} · failed"#,
            ]
        );
        assert!(calls.drain_report().is_empty());
    }

    #[test]
    fn a_call_that_ran_says_how_long_it_took_and_how_much_came_back() {
        let fixture = Fixture::new(Vec::new());
        let executor = Says("one\ntwo");
        let mut calls = fixture.calls(&executor, 10);

        let _ = one(&mut calls, call("read_file", json!({ "path": "a" })));
        let report = calls.drain_report();

        let summary = &report[0];
        let after = summary.rsplit_once(" · ok · ").expect("ok, then time and size").1;
        let (time, size) = after.split_once(" · ").expect("time, then size");
        assert!(time.ends_with("ms") && time[..time.len() - 2].parse::<u64>().is_ok(), "{time}");
        assert_eq!(size, "7 B");
        assert_eq!(&report[1..], ["          one", "          two"]);
    }

    #[test]
    fn calls_of_one_batch_are_joined_and_a_lone_call_is_not() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let _ = calls.answer(reads(&["a", "b", "c"]));
        let _ = calls.answer(reads(&["d"]));
        let _ = calls.answer(reads(&["e", "f"]));

        assert_eq!(
            summaries(&calls.drain_report()),
            [
                r#"  1 ┬ read_file {"path":"a"} · ok"#,
                r#"  2 ├ read_file {"path":"b"} · ok"#,
                r#"  3 └ read_file {"path":"c"} · ok"#,
                r#"  4   read_file {"path":"d"} · ok"#,
                r#"  5 ┬ read_file {"path":"e"} · ok"#,
                r#"  6 └ read_file {"path":"f"} · ok"#,
            ]
        );
    }

    #[test]
    fn a_batch_cut_by_the_call_limit_joins_only_the_calls_it_counted() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 1);

        let _ = calls.answer(reads(&["a", "b", "c"]));

        assert_eq!(
            summaries(&calls.drain_report()),
            [r#"  1   read_file {"path":"a"} · ok"#]
        );
    }

    #[test]
    fn long_arguments_are_cut() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::answering();
        let mut calls = fixture.calls(&executor, 10);

        let _ = one(&mut calls, call("grep", json!({ "pattern": "x".repeat(400) })));
        let summary = calls.drain_report().remove(0);

        let arguments = summary
            .strip_prefix("  1   grep ")
            .and_then(|rest| rest.split(" · ok").next())
            .expect("the arguments");
        assert_eq!(arguments.chars().count(), 201);
        assert!(arguments.ends_with('…'));
    }

    #[test]
    fn a_result_is_previewed_within_its_three_limits() {
        let fixture = Fixture::new(Vec::new());
        let many_lines = (1..=9).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let long_line = "y".repeat(300);
        let heavy = (1..=5).map(|_| "z".repeat(110)).collect::<Vec<_>>().join("\n");

        let preview_of = |text: &'static str| {
            let executor = Says(text);
            let mut calls = fixture.calls(&executor, 10);
            let _ = one(&mut calls, call("read_file", json!({ "path": "a" })));
            calls.drain_report()[1..].to_vec()
        };
        let many = preview_of(Box::leak(many_lines.into_boxed_str()));
        let long = preview_of(Box::leak(long_line.into_boxed_str()));
        let heavy = preview_of(Box::leak(heavy.into_boxed_str()));
        let empty = preview_of("");

        assert_eq!(
            many,
            [
                "          line 1",
                "          line 2",
                "          line 3",
                "          line 4",
                "          line 5",
                "          … 4 more lines",
            ]
        );
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].trim_start().chars().count(), 121, "120 characters and the mark");
        // Five lines of 110 bytes are 550: only three fit in 400.
        assert_eq!(heavy.len(), 4);
        assert_eq!(heavy[3], "          … 2 more lines");
        assert!(empty.is_empty());
    }

    #[test]
    fn a_failed_or_refused_call_is_previewed_by_its_error_and_has_no_size() {
        let fixture = Fixture::new(Vec::new());
        let executor = Recorder::failing("a.txt does not exist");
        let mut calls = fixture.calls(&executor, 10);

        let _ = one(&mut calls, call("read_file", json!({ "path": "a.txt" })));
        let _ = one(&mut calls, call("bash", json!({ "command": "true" })));
        let report = calls.drain_report();

        assert!(report[0].ends_with("ms"), "no size after the time: {}", report[0]);
        assert_eq!(report[1], "          a.txt does not exist");
        assert!(report[2].contains(" · failed · "));
        assert!(report[3].contains("is not available to scripts"));
    }

    #[test]
    fn no_preview_line_can_be_taken_for_a_summary_line() {
        let fixture = Fixture::new(Vec::new());
        let executor = Says("  9   grep {} · ok · 1ms · 2 B\n7 · ok · 3ms");
        let mut calls = fixture.calls(&executor, 10);

        let _ = one(&mut calls, call("read_file", json!({ "path": "a" })));
        let report = calls.drain_report();

        assert_eq!(summaries(&report).len(), 1);
        assert!(report[1..].iter().all(|line| line.starts_with("          ")));
    }
```

In `answers_and_report_follow_the_order_the_calls_were_started_in`, replace the `reported` computation and its assertion with:

```rust
        assert_eq!(
            summaries(&calls.drain_report()),
            [
                r#"  1 ┬ read_file {"path":"a"} · ok"#,
                r#"  2 ├ read_file {"path":"b"} · ok"#,
                r#"  3 └ read_file {"path":"c"} · ok"#,
            ]
        );
```

In `the_call_limit_is_counted_across_a_batch_that_crosses_it`, replace the last assertion with:

```rust
        assert_eq!(
            summaries(&calls.drain_report()).len(),
            2,
            "a call over the limit is not reported"
        );
```

`serde_json` writes object keys in sorted order here (`{"path":"src","pattern":"TODO"}`); if the workspace enables `preserve_order`, the second expected line of `each_call_is_reported_once_with_how_it_ended` is `{"pattern":"TODO","path":"src"}` instead: run the test and use what `serde_json::Value::to_string` gives.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked script::calls`
Expected: FAIL: the new tests fail on the old one-line format (for example `left: ["read_file src/a.rs · ok", …]`).

- [ ] **Step 3: Write each call's lines**

In `crates/orca-runtime/src/script/calls.rs`:

Add below `SCRIPT_TOOL_OUTPUT_BYTES`:

```rust
/// How much of a call the person watching is shown.
const SHOWN_ARGUMENT_CHARS: usize = 200;
const PREVIEW_LINES: usize = 5;
const PREVIEW_LINE_CHARS: usize = 120;
const PREVIEW_BYTES: usize = 400;
/// Deeper than any summary line starts, so the two are never mistaken.
const PREVIEW_INDENT: &str = "          ";
```

Give the two counted variants of `Checked` the call's number:

```rust
    /// Counted, and refused before it ran.
    Refused {
        number: u32,
        error: String,
        checking: Duration,
    },
    /// Counted and allowed: the request to run.
    Allowed {
        number: u32,
        request: ToolRequest,
        checking: Duration,
    },
```

and set it in `check` (`number: self.made` in both arms, after `self.made += 1`).

Replace the loop at the end of `answer` with:

```rust
        // Calls past the limit are not written, so they take no place in the
        // batch as the person watching sees it.
        let written = checked
            .iter()
            .filter(|checked| !matches!(checked, Checked::OverLimit(_)))
            .count();
        let mut place = 0;
        let mut answers = Vec::with_capacity(calls.len());
        for (call, checked) in calls.iter().zip(checked) {
            let (number, answer, took) = match checked {
                Checked::OverLimit(error) => {
                    answers.push(Err(error));
                    continue;
                }
                Checked::Refused {
                    number,
                    error,
                    checking,
                } => (number, Err(error), checking),
                Checked::Allowed {
                    number, checking, ..
                } => {
                    let (result, running) = ran.next().expect("each allowed call was run");
                    (number, answer_from(call, &result), checking + running)
                }
            };
            place += 1;
            self.report.extend(report_lines(
                number,
                batch_mark(place, written),
                call,
                &answer,
                took,
            ));
            answers.push(answer);
        }
        answers
```

Replace `describe` (delete it) with:

```rust
/// What is written for one call: how it ended, then the start of what came
/// back or of why nothing did.
fn report_lines(
    number: u32,
    mark: char,
    call: &ScriptToolCall,
    answer: &Result<String, String>,
    took: Duration,
) -> Vec<String> {
    let (ended, size, text) = match answer {
        Ok(text) => ("ok", format!(" · {}", super::shown_size(text.len())), text),
        Err(error) => ("failed", String::new(), error),
    };
    let mut lines = vec![format!(
        "{number:>3} {mark} {} {} · {ended} · {}ms{size}",
        call.tool,
        cut(&call.arguments.to_string(), SHOWN_ARGUMENT_CHARS),
        took.as_millis()
    )];
    lines.extend(preview(text));
    lines
}

/// Joins the calls of one batch: `┬` opens it, `└` closes it, `├` between.
fn batch_mark(place: usize, of: usize) -> char {
    match (place, of) {
        (_, 0 | 1) => ' ',
        (1, _) => '┬',
        (place, of) if place == of => '└',
        _ => '├',
    }
}

/// The first `chars` characters of `text`, marked when more were left out.
fn cut(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The start of `text`, within the three limits of a preview.
fn preview(text: &str) -> Vec<String> {
    let total = text.lines().count();
    let mut shown = Vec::new();
    let mut bytes = 0;
    for line in text.lines().take(PREVIEW_LINES) {
        let mut line = cut(line, PREVIEW_LINE_CHARS);
        if bytes + line.len() > PREVIEW_BYTES {
            if !shown.is_empty() {
                break;
            }
            // One line of wide characters can pass the byte limit by itself.
            let mut end = PREVIEW_BYTES - '…'.len_utf8();
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line = format!("{}…", &line[..end]);
        }
        bytes += line.len();
        shown.push(format!("{PREVIEW_INDENT}{line}"));
    }
    if total > shown.len() {
        let more = total - shown.len();
        shown.push(format!(
            "{PREVIEW_INDENT}… {more} more line{}",
            if more == 1 { "" } else { "s" }
        ));
    }
    shown
}
```

In `crates/orca-runtime/src/script/mod.rs`, add:

```rust
/// A size as the person watching reads it: `312 B`, `2.1 KiB`.
pub fn shown_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

/// The tools a script may call.
pub fn script_tools() -> &'static [&'static str] {
    &calls::SCRIPT_TOOLS
}

/// The most of one tool result a script is handed.
pub fn script_result_limit_bytes() -> usize {
    SCRIPT_TOOL_OUTPUT_BYTES
}
```

and in `execute_run_script_tool`: take `let started = Instant::now();` on the line before `let wall_clock = …`, and replace the final `show(format!(…))` with:

```rust
    show(format!(
        "{made} tool call{} · {}ms",
        if made == 1 { "" } else { "s" },
        started.elapsed().as_millis()
    ));
```

- [ ] **Step 4: Bring the end-to-end assertions to the new format**

In `crates/orca-runtime/src/tool_turn.rs`:

- in `run_script_reads_a_real_file_and_returns_only_what_the_script_computed`, replace the two `deltas` assertions with:

```rust
        assert!(
            deltas.contains(r#"  1   read_file {"path":"notes.txt"} · ok · "#),
            "{deltas}"
        );
        assert!(deltas.contains("          alpha"), "the result is previewed: {deltas}");
        let last = deltas.lines().last().expect("a last line");
        assert!(last.starts_with("1 tool call · ") && last.ends_with("ms"), "{last}");
```

- in `a_script_stops_at_its_tool_call_limit_and_can_still_answer`, replace `assert!(output_deltas(&events).ends_with("2 tool calls\n"));` with:

```rust
        let deltas = output_deltas(&events);
        let last = deltas.lines().last().expect("a last line");
        assert!(last.starts_with("2 tool calls · ") && last.ends_with("ms"), "{last}");
```

- [ ] **Step 5: Count calls by summary line in the comparison runner**

In `scripts/eval/code_mode/test_run.py` add (it must fail first):

```python
class Summarize(unittest.TestCase):
    def test_a_scripts_calls_are_its_summary_lines_not_its_preview_lines(self):
        chunks = [
            '  1   grep {"pattern":"TODO"} · ok · 12ms · 2.1 KiB\n',
            "          src/a.rs:3: // a · b · ok · 5ms\n",
            "          … 40 more lines\n",
            '  2 ┬ read_file {"path":"a.rs"} · ok · 3ms · 1.2 KiB\n',
            '  3 └ read_file {"path":"c.rs"} · failed · 1ms\n',
            "          c.rs does not exist\n",
            "3 tool calls · 412ms\n",
        ]
        jsonl = "\n".join(
            json.dumps({"type": "tool.output.delta", "payload": {"chunk": chunk}}) for chunk in chunks
        )

        self.assertEqual(run.summarize(jsonl)["script_tool_calls"], 3)
```

(add `import json` to the test file if it is not imported). In `scripts/eval/code_mode/run.py`, add `import re` with the standard-library imports, add after `HERE`'s import block:

```python
# One call of a script as the runtime writes it: number, batch mark, tool,
# arguments, how it ended, how long it took.
SCRIPT_CALL = re.compile(r"^\s*\d+ . \S+ .* · (ok|failed) · \d+ms")
```

and in `summarize` replace the `script_calls +=` line with:

```python
            script_calls += sum(1 for part in chunk.splitlines() if SCRIPT_CALL.match(part))
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked script::
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked run_script
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked a_script_
python3 -m unittest scripts/eval/code_mode/test_expected.py scripts/eval/code_mode/test_run.py
```

Expected: all PASS.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt --all && CARGO_INCREMENTAL=0 cargo clippy -p orca-runtime --all-targets --locked -j 1
git add crates/orca-runtime/src/script/calls.rs crates/orca-runtime/src/script/mod.rs \
  crates/orca-runtime/src/tool_turn.rs scripts/eval/code_mode/run.py scripts/eval/code_mode/test_run.py
git commit -m "feat(runtime): write each script call's arguments, result preview and batch"
```

---

### Task 2: A script run as the conversation shows it

**Files:**
- Create: `crates/orca-tui/src/script_run.rs`
- Modify: the file that declares the TUI crate's modules (`crates/orca-tui/src/lib.rs`; add `mod script_run;` beside the other `mod` lines)

**Interfaces:**
- Consumes: `orca_runtime::script::shown_size` (Task 1).
- Produces, in `crate::script_run`:
  - `pub(crate) const SCRIPT_TOOL: &str = "run_script";`
  - `pub(crate) struct ScriptRun { pub(crate) source: Option<String>, pub(crate) calls: String, pub(crate) end: Option<ScriptEnd>, pub(crate) calls_kept: bool }`, `ScriptRun::live() -> ScriptRun` (calls kept), `ScriptRun::saved(source: Option<String>, end: ScriptEnd) -> ScriptRun` (calls not kept)
  - `pub(crate) struct ScriptEnd { pub(crate) completed: bool, pub(crate) text: String }`
  - `pub(crate) fn script_from_arguments(raw_arguments: &str) -> Option<String>`
  - `ScriptRun::summary(&self) -> Option<&str>`, `ScriptRun::body(&self) -> String`
  - `pub(crate) fn is_section_title(line: &str) -> bool`

This task is pure functions and their tests. Nothing calls them yet, so the module carries `#![allow(dead_code)]` until Task 3 removes it.

- [ ] **Step 1: Write the module with its tests**

Create `crates/orca-tui/src/script_run.rs`:

```rust
//! A `run_script` call as the conversation shows it: the script the model
//! wrote, what its calls did, and what went back to the model.
//!
//! The runtime writes the calls (see `orca_runtime::script`); this module
//! only arranges what it is given. It reads one thing out of that text, the
//! last line, which says how many calls were made and how long the run took.

#![allow(dead_code)]

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

    /// Everything the expanded row shows under its first line.
    pub(crate) fn body(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if let Some(source) = &self.source {
            lines.push(SCRIPT_TITLE.to_string());
            let width = source.lines().count().to_string().len().max(2);
            lines.extend(
                source
                    .lines()
                    .enumerate()
                    .map(|(index, line)| format!("{:>width$} │ {line}", index + 1)),
            );
        }
        if !self.calls_kept {
            lines.push(CALLS_NOT_KEPT.to_string());
        } else {
            let calls: Vec<&str> = self
                .calls
                .lines()
                .filter(|line| !is_run_summary(line))
                .collect();
            if !calls.is_empty() {
                lines.push(CALLS_TITLE.to_string());
                lines.extend(calls.into_iter().map(str::to_string));
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
            lines.extend(end.text.lines().map(|line| format!(" {line}")));
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
/// body starts with a space or a digit, so none can be taken for one.
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

        assert!(body.ends_with("Failed\n Error: boom (script.js:2:7)"), "{body}");
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
        let source = (1..=120).map(|n| format!("line{n}")).collect::<Vec<_>>().join("\n");
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
    fn only_a_title_is_a_title() {
        let body = finished(true, "Script\nCalls\nFailed").body();

        let titles: Vec<&str> = body.lines().filter(|line| is_section_title(line)).collect();
        assert_eq!(titles, ["Script", "Calls", "Returned to the model · 19 B"]);
    }
}
```

Add `mod script_run;` to the TUI crate's module list.

- [ ] **Step 2: Run the tests**

Run: `CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked script_run::`
Expected: 10 passed.

- [ ] **Step 3: Lint and commit**

```bash
cargo fmt --all && CARGO_INCREMENTAL=0 cargo clippy -p orca-tui --all-targets --locked -j 1
git add crates/orca-tui/src/script_run.rs crates/orca-tui/src/lib.rs
git commit -m "feat(tui): arrange a script run's parts into what its row shows"
```

---

### Task 3: The row keeps the script and the calls

**Files:**
- Modify: `crates/orca-tui/src/protocol.rs` (one new `TuiEvent` variant)
- Modify: `crates/orca-tui/src/transcript_state.rs` (one new field on `TranscriptState`)
- Modify: `crates/orca-tui/src/state_reducer.rs` (three arms)
- Modify: `crates/orca-tui/src/surface_projection.rs`, `crates/orca-tui/src/hosted_session.rs` (the two places that build `TuiEvent::ToolRequested`, and `HistoryReplay`)
- Modify: `crates/orca-tui/src/script_run.rs` (remove `#![allow(dead_code)]`)

**Interfaces:**
- Consumes: Task 2's `ScriptRun`, `ScriptEnd`, `script_from_arguments`, `SCRIPT_TOOL`.
- Produces: `TuiEvent::ToolScript { id: String, script: String }`; `TranscriptState.script_runs: HashMap<String, ScriptRun>` keyed by tool call id. For a `run_script` row, `ChatMessage::ToolCall.output` always equals `Some(run.body())` when the body is not empty, and `target` equals `run.summary()`.

Do not add a field to `ChatMessage::ToolCall` or to `TuiEvent::ToolRequested`.

- [ ] **Step 1: Write the failing reducer tests**

Read how the existing reducer tests in `crates/orca-tui/src/state_reducer.rs` (or `state_integration_tests.rs`) build an `AppState` and feed it `TuiEvent`s, and add these tests in the same style, beside the existing tool-row tests. Each uses a helper `script_row(&state) -> (Option<String>, String, Option<String>)` returning the last `ToolCall` row's `(target, status, output)`.

1. `a_script_row_keeps_its_script_and_calls_after_it_ends`: events `ToolRequested { id: "s1", name: "run_script", target: None }`, `ToolScript { id: "s1", script: "return 1;" }`, `ToolOutputDelta { id: "s1", chunk: "  1   git_status {} · ok · 4ms · 9 B\n" }`, `ToolOutputDelta { id: "s1", chunk: "1 tool call · 5ms\n" }`, `ToolCompleted { id: "s1", name: "run_script", status: "completed", output: "1", diff: None, kind: None }`. Expect target `Some("1 tool call · 5ms")`, status `completed`, output `Some("Script\n 1 │ return 1;\nCalls\n  1   git_status {} · ok · 4ms · 9 B\nReturned to the model · 1 B\n 1")`.
2. `a_running_script_row_shows_what_has_arrived`: the first three events only. Expect target `None`, status `running`, output `Some("Script\n 1 │ return 1;\nCalls\n  1   git_status {} · ok · 4ms · 9 B")`.
3. `a_failed_script_row_says_failed`: as 1 but `status: "failed"`, `output: "Error: boom (script.js:1:7)"`. Expect output ending `Failed\n Error: boom (script.js:1:7)`.
4. `a_script_row_without_its_script_still_shows_calls_and_result`: as 1 without the `ToolScript` event. Expect output `Some("Calls\n  1   git_status {} · ok · 4ms · 9 B\nReturned to the model · 1 B\n 1")`.
5. `chunks_of_another_tool_still_go_to_its_output`: `ToolRequested { id: "b1", name: "read_file", target: Some("a.rs") }`, `ToolOutputDelta { id: "b1", chunk: "partial" }`. Expect output `Some("partial")` and target `Some("a.rs")`: unchanged behaviour.
6. `two_script_rows_do_not_share_their_calls`: two interleaved runs `s1` and `s2`, each with its own script and one chunk; each row's output holds only its own script and call.

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked script_row`
Expected: FAIL to compile (`no variant named ToolScript`).

- [ ] **Step 3: Add the event and the store**

In `crates/orca-tui/src/protocol.rs`, add to `TuiEvent`, after `ToolRequested`:

```rust
    /// The script a `run_script` request was made with. Follows the
    /// request's `ToolRequested`.
    ToolScript {
        id: String,
        script: String,
    },
```

In `crates/orca-tui/src/transcript_state.rs`, add to `TranscriptState`:

```rust
    /// The parts of each `run_script` row, by tool call id. The row's
    /// `output` is their arrangement (`ScriptRun::body`).
    pub(crate) script_runs: HashMap<String, crate::script_run::ScriptRun>,
```

Any `match` over `TuiEvent` that the compiler now reports as non-exhaustive gets the arm the neighbouring `ToolOutputDelta` arm has when that arm is a no-op or a pass-through; where `ToolOutputDelta` is handled specially, read the code and do what keeps a `ToolScript` with its `ToolRequested`. List each such place in your report.

- [ ] **Step 4: Keep the row in step in the reducer**

In `crates/orca-tui/src/state_reducer.rs`, add a method on the state type the reducer is implemented for:

```rust
    /// Rewrites a `run_script` row from its run's parts.
    fn refresh_script_row(&mut self, id: &str) {
        let Some(run) = self.transcript.script_runs.get(id) else {
            return;
        };
        let body = run.body();
        let summary = run.summary().map(str::to_string);
        if let Some(index) = self.transcript.messages.iter().rposition(|message| {
            matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == id)
        }) {
            self.mutate_message(index, |message| {
                let ChatMessage::ToolCall { target, output, .. } = message else {
                    unreachable!();
                };
                *output = (!body.is_empty()).then_some(body);
                *target = summary;
            });
        }
    }
```

and three changes in the event `match`:

- a new arm:

```rust
            TuiEvent::ToolScript { id, script } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.transcript
                    .script_runs
                    .entry(id.clone())
                    .or_insert_with(ScriptRun::live)
                    .source = Some(script);
                self.refresh_script_row(&id);
            }
```

- in `TuiEvent::ToolOutputDelta`, before the existing lookup, send a `run_script` row's chunk to its run:

```rust
                let is_script = self.transcript.messages.iter().rev().any(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, name, .. }
                        if existing_id == &id && name == SCRIPT_TOOL)
                });
                if is_script {
                    self.transcript
                        .script_runs
                        .entry(id.clone())
                        .or_insert_with(ScriptRun::live)
                        .calls
                        .push_str(&chunk);
                    self.refresh_script_row(&id);
                    return;
                }
```

- in `TuiEvent::ToolCompleted`, after the existing code has updated or pushed the row and before `learn_command_ends_from_result`, for `name == SCRIPT_TOOL`:

```rust
                if name == SCRIPT_TOOL {
                    self.transcript
                        .script_runs
                        .entry(id.clone())
                        .or_insert_with(ScriptRun::live)
                        .end = Some(ScriptEnd {
                        completed: status == "completed",
                        text: output.clone(),
                    });
                    self.refresh_script_row(&id);
                }
```

Import `crate::script_run::{SCRIPT_TOOL, ScriptEnd, ScriptRun}`. Remove `#![allow(dead_code)]` from `script_run.rs`; if `ScriptRun::saved` is then reported unused, leave the attribute off and add `#[allow(dead_code)]` to that one function until Step 6 uses it.

- [ ] **Step 5: Pass the script on from both projections**

Write the failing tests first, beside the existing projection tests in each file: a `run_script` request with arguments `{"script":"return 1;"}` projects to `ToolRequested` followed by `ToolScript { script: "return 1;" }`; a `read_file` request projects to `ToolRequested` alone; a `run_script` request whose arguments are not JSON projects to `ToolRequested` alone.

- `crates/orca-tui/src/surface_projection.rs`, in the `SurfaceEvent::Tool(ToolPatch::Requested { request })` arm, after the `projected.push(TuiEvent::ToolRequested { … })`:

```rust
                    if request.name.as_str() == crate::script_run::SCRIPT_TOOL
                        && let Some(script) = crate::script_run::script_from_arguments(
                            request.raw_arguments.as_str(),
                        )
                    {
                        projected.push(TuiEvent::ToolScript {
                            id: request.tool_call_id.as_str().to_string(),
                            script,
                        });
                    }
```

- `crates/orca-tui/src/hosted_session.rs`: `runtime_event_to_tui` maps one event to at most one `TuiEvent`, and the `tool.call.requested` payload carries `raw_arguments`. Make a `run_script` request yield both events, `ToolRequested` first, by the smallest change that fits how the function's callers consume its result (read them: if they push the single event onto a list, a companion function `runtime_event_script(event: &EventEnvelope) -> Option<TuiEvent>` called right after is enough). Say in your report what the callers were and what you did.

- [ ] **Step 6: Rebuild a run from a saved conversation**

Write the failing test first, beside the existing `HistoryReplay` tests in `hosted_session.rs`: converting an assistant message that calls `run_script` with `{"script":"return 1;"}` and then its tool message (content `1`, completed) yields a `ToolCall` row named `run_script` whose output is

```text
Script
 1 │ return 1;
Calls are shown only in the session that ran the script
Returned to the model · 1 B
 1
```

and a failed one ends `Failed\n <the error>`. A `read_file` pair is converted exactly as before.

In `HistoryReplay`: add `scripts: HashMap<String, Option<String>>`; in `convert`, when an assistant tool call's `function_name` is `SCRIPT_TOOL`, insert `(call.id.clone(), script_from_arguments(&call.arguments))`; after the existing code has restored the row's name and target for a tool message, if `self.scripts.remove(&id)` gives a script entry, replace the row's `output` with `ScriptRun::saved(source, ScriptEnd { completed: status == "completed", text: <the row's output or empty> }).body()` and its `target` with `None`.

- [ ] **Step 7: Run the tests**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked script
CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked history_replay
```

Expected: the new tests and every existing test matched by those filters PASS. Then run the crate's whole library suite once: `CARGO_INCREMENTAL=0 cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0` and report its summary line.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all && CARGO_INCREMENTAL=0 cargo clippy -p orca-tui --all-targets --locked -j 1
git add crates/orca-tui/src
git commit -m "feat(tui): a run_script row keeps its script and calls beside its result"
```

---

### Task 4: Drawing the row

**Files:**
- Modify: `crates/orca-tui/src/ui.rs` (the `ChatMessage::ToolCall` arm of the function that builds one message's lines, near `append_tool_output_lines`)

**Interfaces:**
- Consumes: Task 2's `is_section_title`, `SCRIPT_TOOL`; Task 3's guarantee about `output` and `target`.
- Produces: nothing other tasks use.

- [ ] **Step 1: Write the failing rendering tests**

Read how the tests in `ui.rs` render a single `ChatMessage::ToolCall` to lines (search the test module for `build_lines_for_messages`) and add, in the same style, with a `run_script` row whose `target` is `Some("3 tool calls · 412ms")`, status `completed` and whose `output` is the body from Task 2's `a_finished_run_shows_its_script_its_calls_and_what_went_back`:

1. `a_collapsed_script_row_is_its_first_line_and_a_way_to_open_it`: `expanded: false`. The rendered text has exactly two rows: one containing `run_script` and `3 tool calls · 412ms`; one containing `+11 lines · click or ctrl+o to expand`. No row contains `const a = 1;`.
2. `an_expanded_script_row_shows_every_section`: `expanded: true`. Rows, in order, contain `Script`, ` 1 │ const a = 1;`, `Calls`, `  2 ┬ read_file`, `Returned to the model · 1 B`; the last row contains `11 lines · ctrl+o to collapse`.
3. `section_titles_are_drawn_in_the_text_colour_and_the_rest_muted`: in the expanded rendering, the span holding `Script` has `fg == theme.text`; the span holding ` 1 │ const a = 1;` has the style `theme.muted_style()`.
4. `a_long_expanded_script_row_is_capped_in_the_live_pane_and_whole_when_flushed`: a body of 300 lines. With `expanded: true, force_expand: false` the tail row contains `200 of 300 lines shown · ctrl+o to collapse`; with `force_expand: true` all 300 lines are present and there is no tail row.
5. `a_running_script_row_with_nothing_yet_is_one_row`: status `running`, `output: None`. One row.
6. `another_tools_row_is_drawn_as_before`: a `read_file` row with a three-line output, collapsed, still shows its first two output lines and `+1 line · ctrl+o to expand` (whatever the existing test for that asserts; copy its expectation).

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked script_row`
Expected: tests 1 to 4 FAIL (the collapsed row shows two lines of the body; no title styling; the cap is 40).

- [ ] **Step 3: Draw a script run's body**

In `crates/orca-tui/src/ui.rs`, next to `append_tool_output_lines`, add:

```rust
/// How many rows of an expanded script run the live pane shows. A run is
/// opened to be read through, so far more than a tool's output gets.
const SCRIPT_RUN_LIVE_ROWS: usize = 200;

/// A script run under its first line: nothing but a way to open it when
/// collapsed; the script, the calls and what went back when expanded.
fn append_script_run_lines(
    lines: &mut Vec<Line<'static>>,
    body: &str,
    expanded: bool,
    force_expand: bool,
    theme: &Theme,
) {
    let body = crate::terminal_output::printable_output(body);
    let total = body.lines().count();
    if total == 0 {
        return;
    }
    if !(expanded || force_expand) {
        lines.push(Line::from(vec![
            Span::styled(TAIL_ROW.to_string(), theme.dim_style()),
            Span::styled(
                format!("+{} · click or ctrl+o to expand", line_count(total)),
                theme.muted_style(),
            ),
        ]));
        return;
    }
    let shown = if force_expand {
        usize::MAX
    } else {
        SCRIPT_RUN_LIVE_ROWS
    };
    let rail = Span::styled("    │ ".to_string(), theme.dim_style());
    for line in body.lines().take(shown) {
        let style = if crate::script_run::is_section_title(line) {
            Style::default().fg(theme.text)
        } else {
            theme.muted_style()
        };
        lines.push(Line::from(vec![
            rail.clone(),
            Span::styled(line.to_string(), style),
        ]));
    }
    if force_expand {
        return;
    }
    let tail = if total > shown {
        format!("{shown} of {total} lines shown · ctrl+o to collapse")
    } else {
        format!("{total} lines · ctrl+o to collapse")
    };
    lines.push(Line::from(vec![
        Span::styled(TAIL_ROW.to_string(), theme.dim_style()),
        Span::styled(tail, theme.muted_style()),
    ]));
}
```

and in the `ChatMessage::ToolCall` arm, where the row's `output` is drawn (`} else if let Some(out) = output {`), make a script run take its own path before the workflow-draft and generic ones:

```rust
            } else if let Some(out) = output {
                if name == crate::script_run::SCRIPT_TOOL {
                    append_script_run_lines(lines, out, *expanded, force_expand, theme);
                } else if !is_workflow_draft_tool(name)
                    || !append_workflow_draft_preview_lines(
```

keeping the rest of that expression as it is. `printable_output`, `TAIL_ROW`, `line_count` and `Theme`'s `dim_style`/`muted_style`/`text` are the ones `append_tool_output_lines` already uses; if any has a different name or signature than used here, follow `append_tool_output_lines`.

`transcript_hit::is_collapsible` already answers true for every `ToolCall`, so the row's header and tail row are hit areas without a change. Confirm by reading it; do not change it.

- [ ] **Step 4: Run the tests**

Run: `CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked script_row`
Expected: all PASS. Then `CARGO_INCREMENTAL=0 cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0` and report its summary line.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all && CARGO_INCREMENTAL=0 cargo clippy -p orca-tui --all-targets --locked -j 1
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): expand a run_script row to its script, calls and result"
```

---

### Task 5: `/scripts`, and the documentation

**Files:**
- Create: `crates/orca-tui/src/script_prompt.rs`
- Modify: `crates/orca-tui/src/commands/mod.rs` (the `SlashCommand` enum, its parser, `all_commands`), `crates/orca-tui/src/slash_command_actions.rs` (one arm), the TUI crate's module list
- Modify: `site/src/docs/md/en/configuration.mdx`, `site/src/docs/md/zh/configuration.mdx`, and the page under `site/src/docs/md/` that lists slash commands (find it: `grep -rln '"/status"\|`/status`' site/src/docs/md`)

**Interfaces:**
- Consumes: `orca_runtime::script::{script_tools, script_result_limit_bytes}` (Task 1); the tool registry (`orca_tools::registry::default_tool_registry().resolve("run_script")`); the session's `RunConfig` (the value `format_status` in `slash_command_actions.rs` is given).
- Produces: `SlashCommand::Scripts`; `pub(crate) fn format_scripts(config: &RunConfig) -> String` (use the config type `format_status` takes).

- [ ] **Step 1: Write the failing tests**

In `crates/orca-tui/src/script_prompt.rs` (new file, tests first):

1. `on_it_says_on_its_limits_and_what_the_model_is_sent`: with a default config whose `tools.script.enabled = true`, the text's first line is `run_script is on`; it contains `60s per script · 100 tool calls · 64 MiB · 8 calls at once · 256 KiB per result`; it contains `a script can call: read_file, glob, grep, git_status`; it contains the line `Description sent to the model` followed by the registered tool's description (compare with the description read from the registry in the test, not with a copied string); it contains `Parameters sent to the model` followed by the registered input schema as pretty-printed JSON, each line indented by two spaces.
2. `off_it_says_how_to_turn_it_on_and_still_shows_what_would_be_sent`: with the default config, the first line is `run_script is off`, the second line contains ``set `enabled = true` under [tools.script] in the user configuration and start a new session``, and the description and parameters are present.
3. `the_limits_are_the_sessions_own`: with `timeout_secs = 5`, `max_tool_calls = 7`, `memory_limit_mb = 16`, `tools.max_read_parallel = 2`, the limits line is `5s per script · 7 tool calls · 16 MiB · 2 calls at once · 256 KiB per result`. With `memory_limit_mb = 0` it says `8 MiB` (the least a script is given).

In `crates/orca-tui/src/commands/mod.rs`, beside the existing parser tests: `parse("/scripts")` is `Some(SlashCommand::Scripts)`, and `all_commands()` contains `("/scripts", "Show what the model is told about run_script")`.

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked scripts`
Expected: FAIL to compile.

- [ ] **Step 3: Write `format_scripts`**

Read the tool spec's field names for the description and the input schema in `crates/orca-tools/src/registry.rs` (the struct `cooperative_builtin_spec` builds). Then write `crates/orca-tui/src/script_prompt.rs`:

```rust
//! What `/scripts` prints: whether `run_script` is offered, its limits, and
//! the description and parameters the model is sent for it. The last two are
//! read from the tool registry, so they cannot drift from what is sent.
```

with `format_scripts` producing, in this order: the state line (and, when off, the how-to line indented by two spaces); the limits line and the `a script can call:` line, each indented by two spaces; an empty line; `Description sent to the model`; the description, wrapped at nothing (one line as registered), indented by two spaces; an empty line; `Parameters sent to the model`; `serde_json::to_string_pretty` of the schema, each line indented by two spaces. The memory figure is `config.tools.script.memory_limit_bytes() / (1024 * 1024)`; the result limit is `orca_runtime::script::script_result_limit_bytes() / 1024`; the tools are `orca_runtime::script::script_tools().join(", ")`.

- [ ] **Step 4: Add the command**

Follow exactly how `SlashCommand::Status` is declared, parsed, listed and handled:

- `commands/mod.rs`: variant `Scripts`; parse `/scripts`; list entry `("/scripts", "Show what the model is told about run_script")` next to `/status`.
- `slash_command_actions.rs`:

```rust
        SlashCommand::Scripts => {
            // Asked for on purpose, like `/status`: show all of it.
            state.push_message(ChatMessage::System {
                text: crate::script_prompt::format_scripts(config),
                expanded: true,
            });
        }
```

If other code matches on `SlashCommand` exhaustively (a help table, a "runs while busy" list), give `Scripts` what `Status` has there and name each place in your report.

- [ ] **Step 5: Document it**

In `site/src/docs/md/en/configuration.mdx`, in the section about scripts, replace the bullet that begins `While a script runs, Orca shows each call it makes` with:

```markdown
- In the terminal UI, a `run_script` row opens with a click or ctrl+o to show the script the model wrote, each call it made (its arguments, how it ended, the first lines of what came back, and which calls ran together) and what went back to the model. The model sees only the script's result, cut at the usual tool output limit. After a session is resumed, the script and the result are still shown; the calls are shown only in the session that ran the script.
- `/scripts` prints whether the tool is on, its limits, and the description and parameters the model is sent for it.
```

and in `site/src/docs/md/zh/configuration.mdx` replace the bullet that begins `脚本运行时，Orca 会显示它的每次调用和结果` with:

```markdown
- 在终端界面里，点击 `run_script` 那一行或按 ctrl+o 可以展开，看到模型写的脚本、它的每次调用（参数、结果、返回内容的前几行，以及哪些调用是并发执行的）和交回给模型的内容。模型只看到脚本的返回值，超出工具输出上限的部分照常截断。恢复会话后仍能看到脚本和返回值；调用过程只在运行脚本的那次会话里显示。
- `/scripts` 会显示这个工具是否开启、它的各项上限，以及发给模型的工具描述和参数定义。
```

Add `/scripts` with the description `Show what the model is told about run_script` (Chinese page: `查看模型收到的 run_script 说明`) to the slash-command list page in both languages, next to `/status`. If no such page exists, say so in your report and add nothing.

- [ ] **Step 6: Run the tests and the gates for this branch**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked scripts
cargo fmt --all -- --check
CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -j 1
CARGO_INCREMENTAL=0 cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0
python3 -m unittest scripts/eval/code_mode/test_expected.py scripts/eval/code_mode/test_run.py
node scripts/test-validate-runtime-surface-contract.mjs && node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-windows-platform-boundaries.mjs && node scripts/validate-windows-platform-boundaries.mjs
node scripts/test-validate-execution-broker-boundary.mjs && node scripts/validate-execution-broker-boundary.mjs
node scripts/test-validate-http-client-boundary.mjs && node scripts/validate-http-client-boundary.mjs
```

Expected: every command exits 0. Report each failing test by name with whether it passes alone; do not retry until green and do not change a test or a validator to pass a gate.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-tui/src site/src/docs/md
git commit -m "feat(tui): /scripts shows what the model is told about run_script"
```
