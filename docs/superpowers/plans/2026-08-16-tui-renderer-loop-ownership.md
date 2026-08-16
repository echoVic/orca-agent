# TUI Renderer Loop Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the complete foreground renderer iteration cycle one private
owner without changing ordering, frame/input/runtime behavior, lifecycle,
protocol, persistence, or visible TUI behavior.

**Architecture:** Add `RendererLoopOwner`, which owns the existing frame owner,
borrows every existing iteration collaborator, and runs the exact loop until
an input exit. Generalize only the crate-private terminal resume helper to the
existing ratatui backend constraint so the owner can be tested with
`TestBackend`.

**Tech Stack:** Rust, ratatui, crossbeam-channel, crossterm, tui-textarea,
Cargo tests, Node contract validators.

---

### Task 1: Spec Gate And RED Owner Tests

**Files:**

- Create: `docs/superpowers/specs/2026-08-16-tui-renderer-loop-ownership.md`
- Create: `docs/superpowers/plans/2026-08-16-tui-renderer-loop-ownership.md`
- Create: `crates/orca-tui/src/renderer_loop.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [x] Audit the inline iteration cycle, lower owners, exact order, limits,
  error/exit behavior, lifecycle/shutdown, tests, contracts, and counts.
- [x] Confirm clean local/remote state, create isolated
  `codex/tui-renderer-loop-owner`, and preserve all unrelated worktrees.
- [x] Write and self-review this Proposed spec and plan before production
  edits, with no TBD/TODO, second reducer, or lifecycle ambiguity.
- [x] Register `mod renderer_loop;` and add owner tests importing the absent
  `RendererLoopOwner`.
- [x] Cover exit-before-presentation, runtime-then-presentation-before-exit,
  and exact clear-error propagation through real lower owners/channels.
- [x] Run `cargo test -p orca-tui renderer_loop --lib --locked -- --test-threads=1`;
  RED must fail only with unresolved `RendererLoopOwner`.

### Task 2: Implement The Foreground Renderer Loop

**Files:**

- Modify: `crates/orca-tui/src/renderer_loop.rs`
- Modify: `crates/orca-tui/src/app.rs:275-370`
- Modify: `crates/orca-tui/src/renderer_frame.rs`
- Modify: `crates/orca-tui/src/presentation.rs`

- [x] Generalize crate-private resume helpers from `InlineTerminal` to
  `Terminal<B>` with the existing `Backend` bound; keep production wiring
  unchanged.
- [x] Add private `RendererLoopOwner` owning `RendererFrameOwner` and borrowing
  the existing iteration collaborators without retaining/cloning events or
  adding a state source.
- [x] Move the exact expired-escape, prepare, wake/resume, acknowledgement,
  dispatch, sync, exit, and presentation sequence into consuming `run`.
- [x] Replace the inline app loop with one owner construction and `run` call;
  keep initial presentation, cleanup, and runtime/inbox/agent shutdown in app.
- [x] Run owner tests GREEN, then focused frame/input-wake/ack/inbox/event/
  runtime/input-router suites.

### Task 3: Close Contracts And Evidence

**Files:**

- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify this spec and plan with measured evidence.

- [x] Add one `renderer_loop` entrypoint with exact app/owner anchors and
  increment the Rust mirror without broadening any baseline.
- [x] Migrate affected lower-owner production anchors to the new loop while
  retaining app construction/shutdown and focused-owner anchors.
- [x] Add deletion self-tests for app construction/run and every owned order/
  result path while masking references remain.
- [x] Regenerate manifest SHA-256 and update only its digest entry.
- [x] Update roadmap owner/count/next-boundary wording, exact source counts,
  implemented evidence, and completed plan checkboxes.
- [x] Run ordinary/test compiler gates, both validators/self-tests, formatter,
  digest equality, and diff check.

### Task 4: Full Verification, Review, Integration, And Cleanup

- [x] Run full serial TUI and root PTY gates.
- [x] Stage the exact slice files and run CodeRabbit over the complete staged
  diff; resolve every valid Critical or Important finding.
- [x] Create one semantic commit: `refactor(tui): own renderer loop`.
- [x] Rebase latest local `main`; repeat owner/affected/validator/full/PTY
  gates.
- [x] Fast-forward local `main`; repeat root owner/validator/full/PTY gates.
- [x] Immediately remove only `.worktrees/tui-renderer-loop-owner` and
  `codex/tui-renderer-loop-owner`, preserve unrelated worktrees, record final
  evidence in the same commit, and verify clean root state.
