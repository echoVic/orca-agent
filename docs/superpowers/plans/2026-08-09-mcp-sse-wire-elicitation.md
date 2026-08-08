# MCP SSE Wire Elicitation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Prove MCP SSE elicitation decline, malformed-request, and cancellation
semantics through real socket exchanges without adding a second lifecycle owner.

**Architecture:** Extend the existing `transport.rs` socket fixtures. Reuse the
current `read_sse_stream`, typed resolver, and worker cancellation path; only
change production code if a wire-level RED test demonstrates a real defect.

**Tech Stack:** Rust, reqwest async SSE worker, blocking transport facade,
`std::net::TcpListener`, existing MCP transport test helpers.

---

### Task 1: RED wire fixtures

**Files:**
- Modify: `crates/orca-mcp/src/transport.rs`

- [x] **Step 1: Add a decline wire fixture.**

Start a TCP listener, send one `elicitation/create` SSE event, read the second
HTTP request, parse its JSON body, and assert the request id is `prompt-decline`
and `result.action` is `decline`. Then send terminal tool result id `1` and
assert the caller returns that result.

- [x] **Step 2: Add a malformed wire fixture.**

Send `elicitation/create` with `params: null`, read the response body, and assert
the original request id plus error code `-32602`. Send terminal tool result id
`1` afterward and assert the caller still returns it.

- [x] **Step 3: Add the stalled elicitation-POST fixture.**

Send a valid elicitation, accept the POST, read its body, then hold the socket
open until EOF or a two-second deadline. Invoke the transport with a cancel
closure that fires after 100ms, assert the existing cancellation error and
bounded return, then join the fixture and assert it observed EOF.

- [x] **Step 4: Run the focused tests and verify RED or existing GREEN.**

Evidence: the three new wire fixtures and the existing elicitation helper tests
pass; no production correction was required.

Run:

```bash
cargo test -p orca-mcp sse_tool_call_routes_elicitation_request_before_final_response --lib -- --test-threads=1
cargo test -p orca-mcp sse_elicitation_decline_is_observed_over_wire --lib -- --test-threads=1
cargo test -p orca-mcp sse_malformed_elicitation_error_is_observed_over_wire --lib -- --test-threads=1
cargo test -p orca-mcp sse_elicitation_post_cancellation_closes_peer_before_returning --lib -- --test-threads=1
```

The new tests may pass immediately because the production path was recently
hardened; if a test fails, preserve the failure as the single implementation
hypothesis before editing production code.

### Task 2: Minimal production correction only if RED identifies one

**Files:**
- Modify: `crates/orca-mcp/src/transport.rs` only when a focused fixture fails

- [x] **Step 1: Trace the failing request id, response body, and worker state.**

Use the fixture assertions and existing parser boundaries to identify whether
the defect is in event routing, typed resolution, POST cancellation, or worker
join. Do not change resource/list parsing or handler APIs as a workaround.

- [x] **Step 2: Add the smallest behavior fix and rerun the single failing test.**

No production failure was observed, so this task changed no implementation code.

Keep terminal id matching, bounded body limits, cancellation-aware futures, and
unconditional worker join intact.

### Task 3: Documentation and gates

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-09-mcp-sse-wire-elicitation.md`
- Modify: `docs/superpowers/plans/2026-08-09-mcp-sse-wire-elicitation.md`

- [x] **Step 1: Record the wire-level acceptance evidence and parser split.**

- [x] **Step 2: Run focused and full affected gates.**

Evidence: `orca-mcp` transport tests passed 32/32; the full `orca-mcp` library
suite passed 48/48; the full `orca-runtime` library suite passed 1040/1040;
formatter and diff checks passed.

```bash
cargo test -p orca-mcp transport --lib -- --test-threads=1
cargo test -p orca-mcp --lib -- --test-threads=1
cargo test -p orca-runtime --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

### Task 4: Review and delivery

- [x] **Step 1: Review for detached workers, mismatched ids, duplicate state,
  and accidental non-tool stream parsing.**

Review result: fixtures join their observation threads; production worker
ownership, terminal id matching, and the existing `tools/call`-only stream
split are unchanged.

- [x] **Step 2: Commit one semantic slice.**

```bash
git add crates/orca-mcp/src/transport.rs docs/production-roadmap.md docs/superpowers/specs/2026-08-09-mcp-sse-wire-elicitation.md docs/superpowers/plans/2026-08-09-mcp-sse-wire-elicitation.md
git commit -m "test(mcp): cover SSE elicitation wire lifecycle"
```

- [x] **Step 3: Rebase current `main`, rerun focused/full gates, and integrate
  with a fast-forward.**

Evidence: rebased on the fetched `origin/main`; focused transport and full MCP
gates remained green after the rebase, then the commit was fast-forwarded into
the main checkout.
