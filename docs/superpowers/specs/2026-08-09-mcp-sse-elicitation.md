# Spec: SSE MCP Elicitation

## Problem and evidence

`crates/orca-mcp/src/transport.rs` accepts an elicitation handler for SSE tool
calls but discards it. The SSE request path buffers one response body and only
accepts a terminal JSON-RPC result, so a server request such as
`elicitation/create` cannot be answered and a tool that waits for that answer
never completes. The stdio transport already routes the same request through
`McpElicitationHandler`.

This is a boundary defect in the MCP transport: server-initiated requests are
not represented in the SSE request lifecycle even though the public transport
trait promises an elicitation handler.

## User value

TUI, server, and headless tool calls behave consistently when a DeepSeek-driven
MCP tool asks for user input over SSE. The call remains pending until the typed
handler decides, then the transport sends the matching JSON-RPC response and
returns the tool's terminal result. Cancellation still closes the in-flight
request and does not leave a worker or waiter behind.

## Scope

- Add SSE response-stream parsing for `tools/call` that yields individual
  `data` messages in order, including server requests before the terminal
  response. Keep initialize/list/resource requests on their existing bounded
  single-response path.
- Route `elicitation/create` through the existing `McpElicitationHandler` and
  POST its accept/decline JSON-RPC response to the configured MCP endpoint.
- Keep the current one-request-per-HTTP-POST behavior, request timeout, body
  bound, headers, id allocation, JSON response compatibility, and cancellation
  semantics.
- Add transport-level tests for accept, decline, malformed requests, and
  cancellation while waiting for a server request.

## Non-goals

- Implementing an independent long-lived GET SSE subscription.
- Changing the `McpTransport` trait or the runtime interaction broker.
- Adding a second elicitation queue or surface-specific state.
- Negotiating session/protocol headers beyond the existing configured headers.

## Behavior contract

1. A JSON response body continues to return its `result` exactly as today.
2. An SSE response is consumed event-by-event. `data:` records are parsed as
   JSON-RPC messages; comments and unrelated notifications are ignored.
3. For `elicitation/create`, the transport constructs the existing typed
   `McpElicitationRequest` (using the configured server name), invokes the
   supplied handler once, and POSTs the matching JSON-RPC response with the
   original request id. Without a handler it sends a decline response.
4. The first terminal response matching the original request id is returned;
   JSON-RPC errors surface as the existing request error. A missing terminal
   response is an explicit error.
5. Cancellation before admission or while reading/awaiting an elicitation
   returns `MCP tool call cancelled`, joins the worker, and does not publish a
   late tool result. A handler decision already in progress may complete before
   cancellation is observed, matching the stdio boundary.
6. Stream and message byte limits remain bounded by `MAX_SSE_RESPONSE_BYTES`.

## Ownership and compatibility

`SseTransport` owns request ids, HTTP clients, configured headers, and the
bounded request worker. The caller-provided `McpElicitationHandler` owns the
decision. The worker owns tool-call stream parsing and sends request
notifications to the caller through a one-shot channel; response writes and
worker joins observe cancellation. No detached worker or process-local pending
store is introduced. Existing MCP JSON, SSE result, CLI/TUI/server, and
persistence contracts remain unchanged.

## Acceptance criteria

- An SSE fixture emits `elicitation/create`, receives an accept response with
  the same JSON-RPC id, then emits a terminal tool result; the transport returns
  that result and the handler observes the typed request.
- Decline and missing-handler paths send `action: decline` and still complete
  the server tool call.
- Malformed server requests fail closed with a diagnostic and do not send an
  untyped response.
- Cancellation while the stream or elicitation response POST is in flight
  returns the cancel error and joins the worker unconditionally.
- Existing `orca-mcp` tests, `cargo fmt --all -- --check`, `git diff --check`,
  workspace MCP/runtime focused tests, and the runtime full gate pass.

## Migration and rollback

The change is additive inside `SseTransport`; no persisted data or public
protocol shape changes. Rollback is a single commit revert. The old buffered
single-message helper can be deleted after the stream parser is green because
all SSE requests will share the new bounded reader.
