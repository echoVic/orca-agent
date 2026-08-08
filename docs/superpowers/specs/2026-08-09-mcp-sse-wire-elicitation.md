# MCP SSE Wire Elicitation Specification

## Problem and Evidence

`crates/orca-mcp/src/transport.rs` already routes `tools/call` SSE events
through the runtime elicitation handler and validates terminal response ids.
The current tests prove the accept path over a socket, but decline, malformed
requests, and cancellation while the elicitation response POST is in flight
are only covered by pure helper tests or a stalled ordinary tool stream. That
leaves the most failure-prone wire and worker-join paths unverified.

## User and Architecture Value

TUI tool calls that require user input must visibly decline malformed or
unhandled requests, preserve the original JSON-RPC correlation, and return
promptly when the owning operation is canceled. The SSE worker remains owned by
the synchronous transport call; no detached reader, second elicitation state,
or client-owned lifecycle is introduced.

## Scope and Non-Goals

In scope:

- a real SSE socket fixture for an unhandled elicitation that observes the
  typed `decline` response before returning a final tool result;
- a real SSE socket fixture for malformed elicitation params that observes a
  JSON-RPC `-32602` error with the original request id;
- a real SSE socket fixture that accepts the elicitation response POST and
  stalls its HTTP response until the client cancels, proving the worker closes
  the peer and the caller returns promptly;
- documentation of the existing split: only `tools/call` consumes event
  streams, while initialize/list/resource methods use bounded terminal JSON/SSE
  parsing.

Out of scope:

- changing MCP wire payloads or adding a new transport abstraction;
- changing handler APIs or making a synchronous handler itself cancellable;
- converting initialize/list/resources to event-stream elicitation;
- adding a background worker or persistent elicitation store.

## Semantics and Ownership

- The transport allocates one request id per request and accepts only a terminal
  response with that id.
- An unhandled valid request receives `{"action":"decline"}` over the wire.
- A malformed request receives JSON-RPC error `-32602` with the elicitation
  request id; the tool call may then continue to its terminal response.
- Cancellation sets the worker cancel flag, drops in-flight response futures,
  joins the worker, and returns the existing cancellation error. The peer that
  accepted the elicitation POST must observe connection closure before the
  caller returns.
- The synchronous caller owns the worker join. The server fixture owns only
  socket observation and never becomes a production lifecycle owner.

## Compatibility

No CLI, TUI, server/JSONL, persistence, MCP payload, or public Rust trait shape
changes. Tests only strengthen the existing SSE behavior and preserve the
bounded single-response path for non-tool requests.

## Acceptance Criteria

1. A wire-level decline fixture sees the original elicitation id and
   `result.action == "decline"`, then the call returns its final tool result.
2. A wire-level malformed fixture sees the original id and error code `-32602`,
   then the call returns its final tool result.
3. A fixture that stalls the elicitation response POST observes the client
   close that POST before `call_tool_with_elicitation_handler_or_cancel` returns
   its cancellation error within the bounded test deadline.
4. Existing MCP transport, runtime, and TUI gates remain green; no production
   worker is detached and no non-tool request enters `read_sse_stream`.

## Verification

```bash
cargo test -p orca-mcp transport --lib -- --test-threads=1
cargo test -p orca-mcp --lib -- --test-threads=1
cargo test -p orca-runtime --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

## Final Evidence

- `cargo test -p orca-mcp transport --lib -- --test-threads=1`: 32 passed.
- `cargo test -p orca-mcp --lib -- --test-threads=1`: 48 passed.
- `cargo test -p orca-runtime --lib -- --test-threads=1`: 1040 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

## Migration and Rollback

This slice is test/doc-only unless the new fixture exposes a lifecycle defect.
Any required production change remains in the same semantic commit and can be
reverted without a data migration or protocol transition.
