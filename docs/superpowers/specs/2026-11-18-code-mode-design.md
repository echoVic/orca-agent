# Code Mode (read-only) Design

- Date: 2026-11-18
- Status: approved in conversation; first version, off by default
- Baseline: `main@c8e3ca32`

## Purpose

Let the model submit one JavaScript snippet that calls read-only tools
directly, so that a search-read-filter investigation costs one model round
instead of one round per tool call, and only what the script returns enters
the context.

The first version exists to measure whether this pays off on DeepSeek. It
calls read-only tools only, so it needs no approval flow of its own. Write
and shell tools, nested display and a default-on switch are decided after
the measurement and are out of scope here.

## What the model sees

One new built-in tool, `run_script`, advertised only when it is enabled (see
Configuration) and Node is available.

```json
{ "script": "const hits = await tools.grep({ pattern: 'TODO' }); return hits;" }
```

`script` is the body of an async function. Inside it:

- `tools.read_file(args)`, `tools.glob(args)`, `tools.grep(args)` and
  `tools.git_status(args)` each take the same arguments object as the tool of
  that name and return a promise of the text that tool returns to the model.
  A call that fails rejects with an `Error` carrying the tool's error text.
- `console.log(...values)` adds a line to the script's output.
- `return value` ends the script. The tool result is the logged lines, then
  the returned value: a string as it is, anything else as indented JSON,
  `undefined` as nothing.

Nothing else is in scope: no `process`, no `require`, no `import()`, no
timers, no state from an earlier `run_script` call.

No other tool is hidden or changed. The advertised tool list is fixed for
the session, as it is today, so the prompt-cache prefix does not move.

## Authority

`run_script` declares the capabilities `FsRead`, `FsList`, `FsSearch` and
`GitInspect`. That makes it a read action: no approval prompt in any mode,
and usable in Plan mode, the same as calling the four tools directly. It is
not `concurrent_safe`, so it takes the normal single-call path, where its
own PreToolUse and PostToolUse hooks run.

Each call a script makes is checked by the runtime, not by the script host:

1. the tool name must be one of the four listed above;
2. the request must also pass `orca_tools::tool_is_available_readonly_concurrent`,
   so a tool that is not read-only is refused even if the list above is ever
   widened by mistake;
3. arguments are validated against the tool's schema;
4. the PreToolUse hook runs for the inner request, and may rewrite or block
   it, as it does in the read-only batch;
5. the call executes under `ExecutionProfile::ReadOnly` through the read-only
   executor (`RuntimeToolCallRuntime::execute_readonly_batch`), the same path
   a direct `read_file` batch takes.

An inner request's id is `<run_script call id>:<n>`, `n` counting from 1.

## Process boundary

The script runs in a Node child process built like the workflow host after
`c8e3ca32`:

- the script's API is created inside the script's own realm; the host
  process and the script exchange JSON text only, and the host never waits
  on a promise of the script's;
- Node is started with code generation from strings disabled and without
  Orca's credentials;
- where Node has the permission model, the process may read the host module
  and the script file and nothing else; it writes nothing and starts nothing.

All file access a script causes happens in the Rust runtime, through the
tools. The Node process never needs the workspace.

The helpers that start a confined Node (`node_confinement_args`,
`node_entry_path`, the child wrapper that kills the process tree) move from
private to `pub(crate)` in `workflow::host` and are reused. The script host
is its own small module file, `crates/orca-runtime/src/script/host.mjs`; it
does not share the workflow host's source scan, export rewriting, mailbox or
task lists.

Running the host inside the OS sandbox is not part of this version. The
sandbox builders take a shell command string and Windows uses a separate
adapter; wiring a non-shell helper process into them is its own piece of
work and applies to the workflow host as well.

## Protocol

Host to runtime, one JSON object per line on stdout:

- `{"type":"tool_call","call_id":"c1","tool":"grep","arguments":{...}}`
- `{"type":"completed","output":"..."}`
- `{"type":"failed","error":"..."}`

Runtime to host, one JSON object per line on stdin:

- `{"type":"tool_result","call_id":"c1","output":"..."}`
- `{"type":"tool_error","call_id":"c1","error":"..."}`

The runtime handles tool calls one at a time in the order they arrive. A
script may start several at once; they are answered in order.

## Limits

| Limit | Default | When reached |
|---|---|---|
| Wall clock for one script | 60 s | process tree killed; tool fails with "script ran longer than 60s" |
| Inner tool calls per script | 100 | the call over the limit rejects in the script; later calls reject too |
| Bytes of one inner result | 256 KiB | the tool's own truncation applies at this size |
| One protocol line | 1 MiB | process tree killed; tool fails |
| Result returned to the model | `tools.output_truncation` (8 KiB) | truncated like any tool output |

Cancelling the turn kills the process tree. A script that never returns is
ended by the wall clock. A failed or killed script has no side effects to
undo: every tool it can call is read-only.

The operation budget counts one tool call per `run_script`. Inner calls are
not journaled as tool starts: the journal's `tool.started` records drive
recovery of calls that may have had effects, and inner calls have none.

## Visibility

Each inner call is reported as a `tool.output.delta` on the `run_script`
call: one line naming the tool, its main argument and how it ended. The last
line gives the number of calls. These lines are for the person watching and
for the session's event log; the model receives only the script's output.

There is no nested tool-call item in the surface today, and this version
adds none.

## Configuration

```toml
[tools.script]
enabled = false        # advertise and allow run_script
timeout_secs = 60
max_tool_calls = 100
```

When `enabled` is false, or Node is not available, `run_script` is not
advertised and a call to it fails with a message that says which.

## Failure behaviour

| Situation | Tool result |
|---|---|
| Script throws | failed: the error's message and stack |
| Syntax error | failed: the syntax error; nothing ran |
| Unknown tool, or a tool outside the four | that call rejects in the script |
| Hook blocks an inner call | that call rejects in the script |
| Node missing or disabled | failed: says which |
| Host exits without finishing | failed: with the host's stderr tail |

## Testing

- Host contract tests with real Node: a script calls tools and returns; a
  rejected call is catchable; `console.log` and return values are assembled;
  realm tests equivalent to the workflow host's (nothing the script holds
  belongs to the host's realm).
- Runtime tests with a fake inner executor: allow-list refusal, schema
  refusal, hook rewrite and block, call limit, wall clock, cancel, line
  limit, ids of inner requests.
- Registry and advertisement tests: the tool is read-only, not concurrent
  safe, hidden unless enabled and Node is available.
- One end-to-end tool-turn test: a `run_script` request reads a real file
  through `tools.read_file` and the result is recorded as the tool message.

## Measurement

A small set of repository-investigation tasks is run with `run_script` off
and on, comparing model rounds, input and output tokens, and cost. The
result decides whether to enable it by default and whether to design write
and shell tools for scripts, which would need per-call approval and a nested
display.
