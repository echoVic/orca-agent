# TUI Pending Interaction Input Admission Plan

**Goal:** Prevent pending user/MCP input from being diverted or irreversibly
consumed before runtime response admission.

**Architecture:** Keep renderer-local text admission in
`idle_submit_actions`, retain MCP mode as a private `AppState` projection, and
leave all response mutation/fencing in `TuiSurfaceTaskControl` and the runtime
surface.

**Tech stack:** Rust, tui-textarea, serde_json, crossbeam channels, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Tests

- [x] Audit submit ordering, slash parsing, pending projection, runtime response
  parsing, optimistic state cleanup, tests, manifest, and lifecycle owners.
- [x] Write the proposed spec before production edits.
- [x] Add submit-owner tests for slash-literal input, invalid Form JSON retry,
  and empty URL acceptance.
- [x] Run the new tests RED for the current ordering/missing mode projection.

## Task 2: Implement Input Admission

- [x] Retain and clear crate-private MCP mode with the pending interaction key.
- [x] Bypass slash parsing while waiting for interaction input.
- [x] Validate MCP JSON before consuming state and admit empty URL as `{}`.
- [x] Preserve all successful user/MCP dispatch behavior and exact payloads.
- [x] Run focused submit, reducer, dispatcher, and canonical interaction tests.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  any required validator evidence.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `fix(tui): preserve pending interaction input`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and promptly remove only
  this worktree/branch.
