# TUI Renderer Iteration Event Routing Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the mixed renderer `IterationEvent` branch one private owner
without changing event ordering, input behavior, runtime reduction, exit/error
propagation, lifecycle, protocol, persistence, or visible TUI behavior.

**Architecture:** Add `RendererIterationEventRouter`, which borrows the same
input/runtime collaborators and delegates one typed iteration event to the
existing `RendererInputRouter` or `RendererRuntimeEventOwner`.

**Tech Stack:** Rust, crossbeam-channel, crossterm, tui-textarea, Cargo tests,
Node contract validators.

---

### Task 1: Spec Gate And RED Owner Tests

**Files:**

- Create: `docs/superpowers/specs/2026-08-16-tui-renderer-event-routing.md`
- Create: `docs/superpowers/plans/2026-08-16-tui-renderer-event-routing.md`
- Create: `crates/orca-tui/src/renderer_event_router.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [x] Audit the inline mixed branch, lower owners, frame bounds/order,
  cancellation/error/exit semantics, lifecycle, tests, contracts, and counts.
- [x] Fetch `origin`, confirm it is not ahead, confirm `.worktrees` is ignored,
  and create isolated `codex/tui-renderer-event-routing` from clean local
  `main`.
- [x] Write and self-review this Proposed spec and plan before production
  edits, with no TBD/TODO, second reducer, or migration ambiguity.
- [x] Register `mod renderer_event_router;` and add owner-level tests importing
  the absent `RendererIterationEventRouter`.
- [x] Cover real runtime Notice delegation, input exit 130 plus Cancel action,
  and exact input clear-error propagation.
- [x] Run `cargo test -p orca-tui renderer_event_router --lib --locked -- --test-threads=1`;
  RED failed only with unresolved `RendererIterationEventRouter`.

### Task 2: Implement The Typed Event Router

**Files:**

- Modify: `crates/orca-tui/src/renderer_event_router.rs`
- Modify: `crates/orca-tui/src/app.rs:328-364`

- [x] Add private `RendererIterationEventRouter` borrowing the existing
  lower-owner collaborators without retaining or cloning events.
- [x] Implement input delegation with the same timestamp and terminal-clear
  callback, returning its `io::Result<Option<i32>>` unchanged.
- [x] Implement runtime delegation exactly once and return `Ok(None)`.
- [x] Replace the inline app match with fresh router construction and one
  `route` call; keep frame ordering/limits, composer sync, exit check, and
  presentation unchanged.
- [x] Run the owner suite GREEN, then focused `renderer_input_router`,
  `renderer_runtime`, and `renderer_frame` suites.

### Task 3: Close Contracts And Evidence

**Files:**

- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify the spec and this plan with measured evidence.

- [x] Add one `renderer_event_routing` entrypoint with exact app and owner
  anchors; increment the Rust mirror without broadening any baseline.
- [x] Add deletion self-tests for app construction/route and owner input/
  result/runtime/continue paths while masks remain.
- [x] Regenerate manifest SHA-256 and update only its digest entry.
- [x] Update roadmap owner/count/next-boundary wording, exact source counts,
  implemented evidence, and completed plan checkboxes.
- [x] Run ordinary/test compiler gates, both validators/self-tests, formatter,
  digest equality, and diff check.

### Task 4: Full Verification, Review, Integration, And Cleanup

- [x] Run full serial TUI and root PTY gates.
- [x] Stage the exact slice files and run CodeRabbit over the complete staged
  diff; resolve every valid Critical or Important finding.
- [x] Create one semantic commit: `refactor(tui): own renderer event routing`.
- [x] Rebase latest local `main`; repeat owner/affected/validator/full/PTY
  gates.
- [x] Fast-forward local `main`; repeat root owner/validator/full/PTY gates.
- [x] Immediately remove only `.worktrees/tui-renderer-event-routing` and
  `codex/tui-renderer-event-routing`, preserve unrelated worktrees, record
  final evidence in the same commit, and verify clean root state.
