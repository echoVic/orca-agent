# Script Engine in Process (PTC stage 1) Design

- Date: 2026-11-19
- Status: approved in conversation
- Baseline: `main@47230d46`
- Replaces the process boundary of `2026-11-18-code-mode-design.md`; the rest
  of that design stands unless this document changes it.

## Purpose

`run_script` starts a Node process for every script. That costs a process
start each time (about 34 ms for an empty Node on the development machine,
before the host module loads), and it makes the tool depend on a Node the
user installed: without one the tool is not offered, so it cannot be on by
default and the tool list differs between machines.

This stage runs the script inside Orca, in an embedded QuickJS, created for
one script and dropped after it. There is no process to start, nothing to
install, and the calls a script starts together run together.

It is the first of four stages toward full programmatic tool calling:

| Stage | What it adds |
|---|---|
| 1. Engine (this document) | Embedded QuickJS, memory and time limits, inner calls run concurrently, no Node |
| 2. Reading | Scripts call search, fetch and read-only MCP tools; tools return structured data |
| 3. Writing | Scripts call `edit`, `write`, `bash` and MCP tools that change things, with per-call approval, nested display, journal recovery and rewind |
| 4. Disclosure and memory | `ALL_TOOLS`, `searchTools`, `describeTool`; `store` and `load`; replay of recorded scripts |

Each later stage has its own design. Nothing in this one decides them.

## What the model sees

The same tool. `run_script` takes `script`, the body of an async function.
In it `tools.read_file`, `tools.glob`, `tools.grep` and `tools.git_status`
take the arguments of the tools of those names and resolve to the text those
tools return; `console.log` adds a line; `return` ends the script. The tool
description gains one sentence, so the model uses what this stage adds:
calls started together, as with `Promise.all`, run at the same time.

One difference a script can observe: `eval` and `Function` exist. The Node
host turned them off because every object the host handed a script led back
to the host's own `Function`, and from there to `process`. Here there is no
host realm to reach. A script already runs whatever JavaScript the model
wrote, and compiling more of it from a string gives it nothing it lacks.

`import()` still fails: no module loader is installed. There are still no
timers and nothing is kept from an earlier call.

## Engine

A new module, `crates/orca-runtime/src/script/engine.rs`, replaces `host.rs`
and `host.mjs`. It depends on the `rquickjs` crate (0.14), which builds
QuickJS from source with the C compiler the build already needs for sqlite,
zstd and ring, and ships generated bindings for all six release targets.

```rust
pub struct ScriptEngineLimits {
    pub wall_clock: Duration,
    pub memory_bytes: usize,
}

pub struct ScriptToolCall {
    pub tool: String,
    pub arguments: serde_json::Value,
}

pub enum ScriptOutcome {
    Completed { output: String },
    Failed { error: String },
}

impl ScriptEngine {
    /// Runs `script` to its end. `answer` is given the calls the script
    /// started together and returns one answer for each, in the same order.
    pub fn run(
        script: &str,
        limits: ScriptEngineLimits,
        answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
        stop: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> io::Result<ScriptOutcome>;
}
```

A cancelled script is an `Interrupted` error, as it is today.

For one script the engine starts a thread of its own, named `orca-script`,
with a 2 MiB stack, and on it:

1. creates a runtime with the engine's own allocator, which enforces the
   memory limit (see "Limits"), sets the script's stack limit to 256 KiB,
   installs an interrupt handler that stops the script when `stop` says so
   or the wall clock has passed, and creates a context holding the
   language's own built-ins and none of the engine's extras (`performance`,
   `atob`, `btoa`, `DOMException`);
2. evaluates the prelude (below);
3. compiles the script as the body of an async function in a file named
   `script.js`, the body starting on line 1, so an error names the line of
   the script as the model wrote it;
4. calls it and runs the loop described under "Concurrency";
5. says how the script ended, and only then drops the context and the
   runtime, so the time that takes is not counted against the script.

The calling thread answers the batches the engine thread hands it (tools run
there, as before) and, while the engine is busy, checks `stop` and the wall
clock every 20 ms. The thread is there because the interrupt handler does not
reach every loop (see "Limits"): `Array.prototype.join` on an object whose
`length` is `2 ** 53` runs inside one native call and never asks it. When the
script must stop, the caller records why (a cancel wins over the clock),
gives the engine 200 ms to notice and end, and then returns the stopped
outcome without waiting for it. An engine thread still busy then is left
behind: it is not joined, it can hand nothing more to the caller, and it
ends when the native call returns, or when Orca exits.

Nothing is injected but what the prelude installs. The engine has no file,
network, process or timer API to take away.

### Prelude

`script/prelude.js`, compiled into the binary, is what `installScriptApi` in
`host.mjs` is today, without the realm boundary it existed to hold. It is one
function expression called with two native functions. It leaves `tools` and
`console` on the global object and removes `queueMicrotask`, the one name
the engine adds to the language's own that the context cannot be built
without.

- `tools.<name>(args)` turns `args` into JSON text, hands the name and the
  text to the native `start_call`, which records the call and returns its
  number, and returns a promise kept under that number. Arguments that are
  not JSON (a cycle, a `BigInt`) reject the call.
- `console.log`, `info`, `warn`, `error` and `debug` add a line: strings as
  they are, anything else as JSON, values joined by a space.
- When the script's function settles, the prelude calls the native `finish`
  with the outcome: the logged lines and then the returned value (a string
  as it is, anything else as JSON indented by two, `undefined` as nothing),
  or the error as `String(error)` followed by the `script.js:line:column`
  from its stack, and the lines logged before it.

The prelude takes `JSON.parse`, `JSON.stringify`, `Promise`, `Error` and
`Map` into locals before the script runs. A script that replaces them breaks
only itself; a script that breaks them so far that the prelude throws fails
with "The script broke the functions it runs on".

The prelude also fixes `Error.prepareStackTrace` as `undefined`, neither
writable nor configurable. A function set there would be handed the frames
beneath an error, each with its function, the prelude's own among them; with
it fixed, a stack is only text.

The engine keeps one prelude function, `deliver(number, ok, text)`, to
resolve or reject a call. The native `finish` keeps the first outcome it is
given and ignores any later one.

## Concurrency

The engine never runs a tool. It runs the script until the script can go no
further, hands over the calls that are waiting, and resumes it with their
answers:

1. Run pending jobs until there are none or the script has said how it
   ended.
2. If the script has finished, return its outcome, even if jobs it started
   are still pending.
3. If calls were started since the last hand-over, give them to `answer` as
   one batch in the order they were started, deliver each answer in that
   order, and go to 1.
4. Otherwise the script is waiting on something nothing will ever settle:
   fail with "the script is waiting on a promise that nothing will resolve".

`await a; await b;` is two batches of one. `await Promise.all([a, b, c])` is
one batch of three.

`ScriptToolCalls` in `calls.rs` answers a batch:

- Each call is checked in the order it was started, on the calling thread:
  the call limit, the four-name list, `tool_is_available_readonly_concurrent`,
  the schema, and the `PreToolUse` hook, which may rewrite or block it. These
  are the checks made today, unchanged. An inner request's id is still
  `<run_script call id>:<n>`, `n` counting from 1 in start order.
- The calls that passed run at the same time on scoped threads, at most
  `tools.max_read_parallel` (8 by default) at once. A batch of one runs on
  the calling thread.
- Answers go back in start order whatever order the tools finished in. A
  result over 256 KiB is still refused rather than handed over cut.
- The lines shown to the person watching are emitted in start order when the
  batch ends, each with its own tool's time.

`ScriptToolExecutor` gains a `Sync` bound. The four tools already run on
worker threads in the read-only batch.

The probe the executor passes to a tool answers true when the turn is
cancelled or the script's time is up. Each of the four tools asks it:
`read_file` between the pieces of the file it reads, `glob` at each entry it
walks, and `grep` and `git_status` every 50 ms while the `rg` or `git` they
started runs, which they then end. A call stopped this way is answered as
cancelled. The tools ask the same question when the model calls them
directly, so a cancelled turn no longer waits for a search to finish.
(At first only `read_file` asked; the other three were changed after this
stage.)

A worker asks the executor's `stopped()` (the same question) before it
starts each call. Once the script is stopped, the calls of the batch not yet
started are not started: each is answered as failed with "the script was
stopped before this call started" and takes no time. Calls already running
finish.

## Limits

| Limit | Default | When reached |
|---|---|---|
| Wall clock for one script | 60 s (`timeout_secs`) | script stopped; tool fails with "the script ran longer than 60s and was stopped" |
| Inner tool calls per script | 100 (`max_tool_calls`) | the call over the limit rejects in the script; later calls reject too |
| Bytes of one inner result | 256 KiB | the call rejects, telling the script to ask for less |
| Memory of one script | 64 MiB (`memory_limit_mb`, never less than 8) | the allocation fails in the script; if the script then fails, whatever it threw, the tool fails with "the script used more than 64 MiB" |
| Stack of one script | 256 KiB, set by the engine, on its own 2 MiB thread | a `RangeError` in the script, which it may catch |
| Argument text of the inner calls started and not yet answered | 256 KiB of JSON text in all | the call that would pass it rejects in the script with "tool call arguments waiting to be answered are limited to 256 KiB in all; await some calls before starting more" |
| Inner calls waiting to be handed over | 1000 | the call rejects in the script with "too many tool calls are waiting; await some before starting more" |
| Result returned to the model | `tools.output_truncation` | truncated like any tool output |

The 1 MiB protocol line limit is gone with the protocol. What it bounded
inside the script, a script building one enormous value, is bounded by the
memory limit. What a script hands over is not: the arguments of a call are
held outside the script's memory until the batch is handed over, as the
JSON text the script made, and are parsed only then. So they have their own
limits: the calls a script has started and not yet been answered may hold
at most 256 KiB of argument text in all, and at most 1000 calls may be
waiting.

The engine knows a script ran out of memory from its allocator, not from
what the script threw. When QuickJS cannot allocate even the error it
throws `null`, and a script can throw an `InternalError("out of memory")`
of its own, so the text says nothing either way. The runtime is created with
`Runtime::new_with_alloc` and an allocator of the engine's own that wraps the
binding's `RustAllocator`. It counts what each block takes, refuses an
allocation that would pass the limit, and sets a flag when it does.
QuickJS's own limit is left off, so every refusal is one the allocator saw.
A script that fails after a refusal, whether by throwing, by breaking the
functions it runs on, or by waiting on a promise nothing will resolve, is
reported as over the limit; a script that catches the refusal and returns
has completed.

That interface is part of `rquickjs` 0.14's default build, so no feature is
enabled. The `allocator` feature is deprecated in 0.14 and adds nothing;
enabling it only adds a deprecation warning. `rust-alloc` changes what
`Runtime::new` allocates with, which the engine does not call. A test
allocates past the limit and expects the failure, so a change that disables
the limit fails the build's tests.

The interrupt handler stops code that runs as steps of the script: a
`while (true)`, a promise chain that reschedules itself forever, a regular
expression that backtracks without end. QuickJS asks it between steps and
inside a regular-expression match, not inside every native function that
loops on its own: among the loops the interrupt does not reach are `join`
and `reverse` over an object whose `length` is `2 ** 53` and
`String.prototype.indexOf` over very long strings. Those the caller's thread
stops: it stops waiting for the engine at the wall clock or the cancel, as
described under "Engine". Neither way is catchable by the script.

The binding's documentation does not say that the handler is consulted
inside a regular-expression match. A prototype of this engine settled it:
`/^(a+)+$/` against forty `a`s and a `!`, with a 400 ms wall clock, was
stopped at 400 ms. The same script is one of the hostile-script tests.

## What in-process costs

The Node host was a second process: whatever went wrong in it stayed there.
A defect in QuickJS itself (a crash in its C code on a script the model
wrote) now ends the Orca process. Nothing can be done about that from Rust;
what is done is to keep a script from reaching the states most likely to
find one: the memory limit, the stack limit, the interrupt, and a set of
hostile scripts in the tests (below).

The engine runs on a thread of its own so that a script stuck in a native
loop cannot hold the turn. The cost is that such a script is not ended, only
left: its thread keeps spinning, and keeps the memory its runtime holds (up
to the memory limit), until the native call returns or Orca exits. Some such
calls take hours. Each script left this way costs a thread until then.

A Rust panic on the engine thread is caught there and becomes a failed tool
result, "run_script failed inside the script engine". So does a panic in
`answer` on the calling thread, which also stops the script. Tools run on the
calling thread while the engine thread waits for their answers, never
beneath a QuickJS frame, so a panic in a tool does not unwind through C. The
two native functions the prelude calls only record what they are given.

A failed or stopped script has nothing to undo: every tool it can call is
read-only.

Measured on the prototype, on the development machine: 200 scripts created,
run and dropped in 143 ms in a debug build, and a release binary that only
evaluates one expression is 1.1 MB larger with the engine than without.

## Configuration

```toml
[tools.script]
enabled = false        # see "Measurement" for when this becomes true
timeout_secs = 60
max_tool_calls = 100
memory_limit_mb = 64   # new; at least 8
```

The table stays user-owned: a project's `.orca/config.toml` cannot set it.

`run_script` is offered when `enabled` is true and for no other reason. It
is read at session start and the offered list does not change mid-session.

## What is removed

- `script/host.rs`, `script/host.mjs`, the line protocol and its 1 MiB
  limit, the temporary files for host and script.
- `script::node_available`, the `node_available` parameter of
  `omit_unavailable_session_tools`, the "needs Node.js" failure, and the
  early returns in tests that skipped without Node.
- The script host's launch through the execution broker and its
  `remove_credential_env` call: no process is started.
- `pub(crate)` on the workflow host helpers opened for the script host
  (`WorkflowHostFileGuard`, `WorkflowHostChild`, `read_bounded_workflow_frame`,
  `spawn_retained_reader`, `node_entry_path`, `node_confinement_args`), where
  the workflow host is again their only user.

The workflow host is not touched. Workflows still need Node.

The Windows platform boundary manifest loses its
`script-host-unix-process-api` row in the same change. The runtime surface
contract and the execution broker boundary name nothing of the script host;
their validators are run to confirm it.

## Failure behaviour

| Situation | Tool result |
|---|---|
| Script throws | failed: the error and `script.js:line:column`, then any logged lines |
| Syntax error | failed: the syntax error; nothing ran |
| Unknown tool, or a tool outside the four | that call rejects in the script |
| Hook blocks an inner call | that call rejects in the script |
| A call that would take the waiting calls past 256 KiB of argument text in all, or 1000 calls already waiting | that call rejects in the script |
| Wall clock passed, including a script that finished after it | failed: says how long it ran |
| Memory limit passed, and the script then fails | failed: says the limit |
| Turn cancelled, including a script that finished after the cancel | cancelled |
| Script waits on a promise nothing resolves | failed: says so |
| `run_script` turned off | failed: says how to turn it on |
| A panic in the engine | failed: "run_script failed inside the script engine" |

## Testing

- **Engine contract** (`tests/script_engine_contract.rs`, from
  `tests/script_host_contract.rs`): the tests that describe behaviour carry
  over with the same assertions: a script returns what it computed; every
  tool reaches the runtime under its own name; logged lines precede the
  returned value; a script returning nothing has only its log; a failed call
  rejects and can be caught; a thrown error says where; a syntax error runs
  nothing; a script that never returns is stopped at the wall clock;
  cancelling stops a running script. They no longer need Node and run on
  every CI machine. The three that tested the Node boundary are replaced:
  a script sees no `process`, `require`, `fetch`, `setTimeout` or `import()`;
  the global object holds `tools`, `console` and the language's own names
  and nothing else.
- **Batches**: calls started together arrive as one batch in start order;
  sequential awaits arrive one at a time; answers delivered out of finishing
  order still resolve the right promises.
- **Hostile scripts**: `while (true) {}`; a promise chain that reschedules
  itself forever; unbounded recursion; allocating past the memory limit, in
  large and in small pieces; a regular expression that backtracks without
  end; `join` and `reverse` over an object whose `length` is `2 ** 53` and
  `String.prototype.indexOf` over very long strings; a script that closes
  the function it is the body of and loops outside it; a script that sets
  `Error.prepareStackTrace`; a script that replaces `Promise`, `JSON` and
  `Object.prototype` members. Each ends within its limit and the next
  script runs normally. The native loops cannot be ended, only left, so
  their test leaves threads spinning for the rest of the test run.
- **Runtime** (`calls.rs`): the existing tests, plus: three slow calls in one
  batch take about as long as one; no more than `max_read_parallel` run at
  once; a blocked call in a batch does not stop the others; report lines and
  request ids follow start order; the call limit counts across a batch that
  crosses it.
- **Advertisement**: offered exactly when enabled; the registry test for the
  read-only, not-concurrent-safe registration stays.
- **End to end**: the tool-turn test that reads a real file through
  `tools.read_file` runs without the Node check.
- The four validators pass before the branch is pushed.

## Measurement

`scripts/eval/code_mode/run.py` runs five repository questions with
`run_script` off and on and records rounds, tokens and cost. It does not
say whether an answer is right, and that is what the default depends on.

- Each task gains a checker that computes the expected answer from the
  checked-out tree (the files and counts a `grep` gives) and marks an answer
  right when it names every expected item with the right count. Expected
  answers are computed, not stored: this change itself removes one
  `launch_user_trusted` call site and one `remove_credential_env` call site,
  which two of the tasks count.
- Each task runs three times off and three times on, through `orca exec`
  against the real provider: 30 runs.
- `enabled` becomes `true` by default when, over the 30 runs, answers marked
  right with the tool on are at least as many as with it off, and both the
  total of model rounds and the total cost are lower with it on. Otherwise
  it stays `false`.

Either way the table of results is added to this document under "Results"
and to the release note, with the release build's size before and after on
`aarch64-apple-darwin`.

If the default becomes `true`, every session's tool list gains one tool
compared with the previous release. The list is still fixed for a session,
so the prompt-cache prefix does not move mid-session. `orca config show`
reports the setting, and `enabled = false` turns it off.

## Out of scope

- Any tool beyond the four, structured results, tools that change things,
  tool discovery, state between scripts, replay: stages 2 to 4.
- The workflow host.
- Running anything inside the OS sandbox: a script starts no process.

## Results

Measured on 2026-11-19 at `15e93b0e`, five tasks, three runs each way, through
`orca exec` against the real provider. All 30 runs ended normally.

| | `run_script` off | `run_script` on |
|---|---|---|
| Runs | 15 | 15 |
| Answers right | 15 | 15 |
| Model rounds | 53 | 57 |
| Cost (USD) | 0.018668 | 0.017745 |

With the tool on, the model called `run_script` in 3 of the 15 runs (5 calls
in all); in the other 12 it used `grep`, `read_file` and `bash` as it does
with the tool off. The runs that used it took no fewer rounds.

Release binary on `aarch64-apple-darwin`: 71348400 bytes before, 72367760
bytes after (1019360 more).

The comparison is inconclusive. The model chose `run_script` in 3 of the 15
runs with it on, and in the saved runs four of the tasks took two to four
rounds and the fifth five to eight, which leaves a script little to save.
With three runs per cell, 53 rounds against 57 is within the spread between
runs of the same task. For every task the runs with the tool off came before
those with it on, so a warmer prompt cache may have favoured the runs with
it on. The data shows neither a saving nor a cost. `enabled` stays `false`
by default because nothing was shown, not because the tool did worse.

Nine right answers were first marked wrong by the checker, which took a file
or an event mentioned in a sentence for one the answer listed, and line
numbers for stated totals. The checker was corrected and the saved answers
marked again; no run was repeated.
