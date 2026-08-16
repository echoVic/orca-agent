# TUI Renderer Runtime Inbox Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the renderer runtime-event receiver and its close boundary one
private owner without changing mailbox capacity, event scheduling, reduction,
shutdown behavior, protocol, persistence, or visible TUI behavior.

**Architecture:** Add `RendererRuntimeInboxOwner`, which consumes the existing
bounded receiver, exposes its borrowed non-blocking iterator, and explicitly
drops it between mention-search shutdown and hosted-agent shutdown.

**Tech Stack:** Rust, crossbeam-channel, Cargo tests, Node contract validators.

---

### Task 1: Spec Gate And RED Owner Tests

**Files:**

- Create: `docs/superpowers/specs/2026-08-16-tui-renderer-runtime-inbox.md`
- Create: `docs/superpowers/plans/2026-08-16-tui-renderer-runtime-inbox.md`
- Create: `crates/orca-tui/src/renderer_runtime_inbox.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [x] Audit receiver construction/capacity, frame batching, single-event
  reduction, producer backpressure, shutdown order, tests, contracts, and
  exact counts.
- [x] Create isolated `codex/tui-renderer-runtime-inbox` from clean local
  `main` and write this Proposed spec and plan before production edits.
- [x] Register `mod renderer_runtime_inbox;` and add owner-level tests that
  import the absent `RendererRuntimeInboxOwner`.
- [x] Cover empty/disconnected iteration, FIFO and partial consumption,
  post-construction sends, and full-mailbox producer release on shutdown.
- [x] Run `cargo test -p orca-tui renderer_runtime_inbox --lib --locked -- --test-threads=1`;
  RED failed only with unresolved `RendererRuntimeInboxOwner`.

### Task 2: Implement The Inbox Owner

**Files:**

- Modify: `crates/orca-tui/src/renderer_runtime_inbox.rs`
- Modify: `crates/orca-tui/src/app.rs:150,252,323,404`

- [x] Add private `RendererRuntimeInboxOwner` retaining the existing
  `TuiEventReceiver` without cloning it.
- [x] Implement borrowed non-blocking `pending()` through `try_iter()` and a
  consuming `shutdown()` that explicitly drops the receiver.
- [x] Construct the owner from `pending_event_rx`, pass `pending()` to the
  existing frame iteration, and replace only raw receiver drop with owner
  shutdown.
- [x] Preserve input-before-runtime order, capacity-derived runtime cap,
  mention-owner-before-inbox-before-agent shutdown, error propagation, and all
  routing collaborators.
- [x] Run the owner suite GREEN, then focused `channels`, `renderer_frame`,
  `renderer_runtime`, and `agent_runtime` suites.

### Task 3: Close Contracts And Evidence

**Files:**

- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify the spec and this plan with measured evidence.

- [x] Add one `renderer_runtime_inbox` entrypoint with exact app and owner
  anchors; increment the Rust mirror without broadening any baseline.
- [x] Add deletion self-tests for app construction/pending/shutdown order and
  owner receiver/try-iteration/explicit-drop paths while masks remain.
- [x] Regenerate manifest SHA-256 and update only its digest entry.
- [x] Update roadmap owner/count/next-boundary wording, exact source counts,
  implemented evidence, and completed plan checkboxes.
- [x] Run ordinary/test compiler gates, both validators/self-tests, formatter,
  digest equality, and diff check.

### Task 4: Full Verification, Review, Integration, And Cleanup

- [x] Run the pre-review full serial TUI and PTY gates: 1,139/1,139 and 6/6.
- [x] Stage the exact 11 slice files and run CodeRabbit over the complete
  staged diff; it reported zero findings.
- [x] Create one semantic commit: `refactor(tui): own renderer runtime inbox`.
- [x] Rebase latest local `main` (already up to date); repeat owner 3/3,
  affected 17/17, validators/self-tests, full TUI 1,139/1,139, and PTY 6/6.
- [x] Fast-forward local `main`; repeat root owner 3/3, validators/self-tests,
  full TUI 1,139/1,139, and PTY 6/6.
- [x] Immediately remove only `.worktrees/tui-renderer-runtime-inbox` and
  `codex/tui-renderer-runtime-inbox`; preserve unrelated worktrees and record
  final evidence in the same semantic commit.
