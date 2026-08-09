# Server JSONL Tool Approval Attachment Specification

## Problem and Evidence

The real DeepSeek server approval-recovery harness reaches a permission
approval, then fails the following `write_file` approval with `no runtime
surface can answer tool approval`. The server turn attachment in
`orca-runtime/src/server/surface_adapter.rs` requests permission, user-input,
and MCP elicitation interactions but omits `ToolApproval`, even though the
production JSONL authority advertises that interaction kind.

## User and Architecture Value

Server/JSONL turns must route every interactive decision through their one
attached runtime surface. The runtime surface hub remains the sole attachment
selector and interaction owner; this fix only makes the server attachment
declare the capability it already promises.

## Scope and Non-Goals

In scope:

- request `ToolApproval` on persistent and stateless server turn attachments;
- add a regression asserting both attachment paths expose the interaction kind;
- rerun the focused server/runtime tests and real approval-recovery harness.

Out of scope:

- changing approval policy, permission semantics, event schemas, or client
  response handling;
- changing read-only server attachments used by history/settings/resources;
- broad server protocol refactors.

## Semantics and Ownership

- `SurfaceHub` selects the attached JSONL surface for `ToolApproval`.
- `surface_adapter` owns the attachment declaration for each server turn.
- The existing operation fence and interaction waiter continue to own lifecycle,
  response routing, and join/retirement behavior.
- No fallback denial is considered success when a server turn has an attached
  JSONL interaction transport.

## Compatibility

This is additive to server JSONL interaction capabilities. Existing permission,
TUI, ACP, CLI, persistence, and read-only server paths are unchanged. Clients
that never receive a tool approval request observe no protocol change.

## Acceptance Criteria

1. Persistent and stateless server turn attachments request `ToolApproval`.
2. A focused regression fails before the fix and passes after it.
3. The real DeepSeek server approval-recovery harness answers the initial
   permission request and the routed `write_file` tool approval, completes the
   write, and reports `ORCA_SERVER_APPROVAL_RECOVERY_REAL_OK`.
4. Focused runtime/server tests, formatting, and diff checks pass.

## Verification

```bash
cargo test -p orca-runtime server::surface_adapter --lib -- --test-threads=1
cargo test --test runtime_lifecycle_contract -- --test-threads=1
node scripts/release/real-api-server-approval-recovery.mjs --bin "$PWD/target/debug/orca" --timeout-ms 180000
cargo fmt --all -- --check
git diff --check
```

## Migration and Rollback

No persisted data or schema changes. Reverting the capability declaration
restores the previous behavior without migration.
