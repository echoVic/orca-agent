# Plan: SSE MCP Elicitation

## Step 1: RED contract

- Add a local SSE fixture that keeps the POST response open, emits an
  `elicitation/create` event, waits for the matching JSON-RPC response, and
  then emits the terminal tool result.
- Assert the typed handler request, request id, accept/decline payload, and
  returned tool result. Add cancellation coverage while the fixture waits.
- Run the focused `orca-mcp` test and confirm it fails because the current
  implementation ignores the handler and concatenates multiple SSE messages.

Status: complete. The focused fixture failed with
`failed to read MCP SSE response: request or response body error` while the
server waited for the missing elicitation response.

## Step 2: Implement the owned stream path

- Store the configured MCP server name in `SseTransport`.
- Replace the `tools/call` buffered SSE reader with a bounded event parser that
  can surface server requests before the final response. Keep initialize,
  list, and resource calls on the existing bounded single-response helper.
- Add a worker-to-caller elicitation channel and send JSON-RPC responses through
  the same endpoint, preserving cancellation and joins.
- Keep JSON response bodies and non-tool SSE calls compatible through the
  existing single-response helper, while routing tool calls through the parser
  with an optional handler channel.

Status: complete. `SseTransport` owns the bounded worker and stream parser;
the caller thread invokes the existing handler and returns one typed decision
through a one-shot response channel.

## Step 3: GREEN and cleanup

- Run focused `orca-mcp` transport tests, then all `orca-mcp` tests.
- Add concise comments only around the stream framing/handler handshake.
- Update `docs/production-roadmap.md` to record the SSE parity slice and its
  evidence.

Status: complete. All 45 `orca-mcp` library tests, 34 runtime surface
interaction contracts, and 8 runtime MCP interaction tests pass.

## Step 4: Verification and review

- Run `cargo fmt --all -- --check` and `git diff --check`.
- Run the affected runtime/MCP contract tests and the full `orca-runtime` lib
  gate because this changes a shared tool transport.
- Review the diff for ownership, cancellation during response POST, bounded
  reads, response-id validation, protocol compatibility, and unconditional
  worker joins.
- Commit the semantic slice, rebase onto fresh `origin/main`, and rerun the
  focused and full affected gates.

Status: complete. Focused transport tests, the full `orca-mcp` library suite,
runtime interaction contracts, strict scoped Clippy, formatter, and diff
checks pass. The suite does not yet include a wire-level decline fixture or a
stalled elicitation-response POST cancellation fixture; the resolver and
cancel-aware implementation paths are covered by unit and existing transport
tests.
