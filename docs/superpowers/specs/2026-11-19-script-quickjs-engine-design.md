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

Unchanged. One tool, `run_script`, taking `script`, the body of an async
function. In it `tools.read_file`, `tools.glob`, `tools.grep` and
`tools.git_status` take the arguments of the tools of those names and
resolve to the text those tools return; `console.log` adds a line; `return`
ends the script. The tool description loses nothing but the reason it could
be missing.

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

For one script the engine:

1. creates a runtime and a context on the calling thread, sets the memory
   limit, and installs an interrupt handler that stops the script when `stop`
   says so or the wall clock has passed;
2. evaluates the prelude (below);
3. compiles the script as the body of an async function in a file named
   `script.js`, the body starting on line 1, so an error names the line of
   the script as the model wrote it;
4. calls it and runs the loop described under "Concurrency";
5. drops the context and the runtime.

Nothing is injected but what the prelude installs. The engine has no file,
network, process or timer API to take away.

### Prelude

`script/prelude.js`, compiled into the binary, is what `installScriptApi` in
`host.mjs` is today, without the realm boundary it existed to hold. It is one
function expression called with two native functions and it leaves `tools`
and `console` on the global object and nothing else.

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

The engine keeps one prelude function, `deliver(number, ok, text)`, to
resolve or reject a call.

## Concurrency

The engine never runs a tool. It runs the script until the script can go no
further, hands over the calls that are waiting, and resumes it with their
answers:

1. Run pending jobs until there are none.
2. If the script has finished, return its outcome.
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

The script's wall clock now holds inside a tool call as well: the probe the
executor passes to a tool answers true when the turn is cancelled or the
script's time is up.

## Limits

| Limit | Default | When reached |
|---|---|---|
| Wall clock for one script | 60 s (`timeout_secs`) | script stopped; tool fails with "the script ran longer than 60s and was stopped" |
| Inner tool calls per script | 100 (`max_tool_calls`) | the call over the limit rejects in the script; later calls reject too |
| Bytes of one inner result | 256 KiB | the call rejects, telling the script to ask for less |
| Memory of one script | 64 MiB (`memory_limit_mb`) | the allocation fails in the script as an out-of-memory error; uncaught, the tool fails with "the script used more than 64 MiB" |
| Stack of one script | 256 KiB (QuickJS's default) | a stack-overflow error in the script |
| Result returned to the model | `tools.output_truncation` | truncated like any tool output |

The 1 MiB protocol line limit is gone with the protocol. What it bounded, a
script building one enormous value, is bounded by the memory limit.

`rquickjs` must be built without its `rust-alloc` and `allocator` features:
with either, the memory limit is silently ignored. A test allocates past the
limit and expects the failure, so a feature change that disables the limit
fails the build's tests.

The interrupt handler is what stops code that never yields: a `while (true)`,
a promise chain that reschedules itself forever, a regular expression that
backtracks without end. Stopping is not catchable by the script.

That the handler is consulted inside a regular-expression match is not
stated by the binding's documentation. The hostile-script test for it
settles the question; if it fails, the default does not change until a
script can no longer hold the turn past its wall clock that way.

## What in-process costs

The Node host was a second process: whatever went wrong in it stayed there.
A defect in QuickJS itself (a crash in its C code on a script the model
wrote) now ends the Orca process. Nothing can be done about that from Rust;
what is done is to keep a script from reaching the states most likely to
find one: the memory limit, the stack limit, the interrupt, and a set of
hostile scripts in the tests (below).

A Rust panic while a script runs is caught at the engine's entry and becomes
a failed tool result. Tools run between job runs, never beneath a QuickJS
frame, so a panic in a tool does not unwind through C. The two native
functions the prelude calls only record what they are given.

A failed or stopped script has nothing to undo: every tool it can call is
read-only.

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

Three reviewed manifests change with this and are updated in the same
change: the runtime surface contract (`script::host` leaves the surface and
`script::engine` enters it), the Windows platform boundary manifest (the
script host's process API registration goes), and the execution broker
boundary list (one `launch_user_trusted` call site fewer).

## Failure behaviour

| Situation | Tool result |
|---|---|
| Script throws | failed: the error and `script.js:line:column`, then any logged lines |
| Syntax error | failed: the syntax error; nothing ran |
| Unknown tool, or a tool outside the four | that call rejects in the script |
| Hook blocks an inner call | that call rejects in the script |
| Wall clock passed | failed: says how long it ran |
| Memory limit passed, uncaught | failed: says the limit |
| Turn cancelled | cancelled |
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
  itself forever; unbounded recursion; allocating past the memory limit; a
  regular expression that backtracks without end; a script that replaces
  `Promise`, `JSON` and `Object.prototype` members. Each ends as a failed
  result within its limit and the next script runs normally.
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
