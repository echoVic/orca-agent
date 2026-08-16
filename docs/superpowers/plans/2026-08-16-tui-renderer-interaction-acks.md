# TUI Renderer Interaction Acknowledgement Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give renderer-side interaction acknowledgement receiver and drain
semantics one private owner without changing acknowledgement production,
single-item reduction, frame scheduling, protocol, persistence, or visible
TUI behavior.

**Architecture:** Add `RendererInteractionAckOwner`, which owns the existing
receiver clone and drains its currently queued items through
`handle_interaction_response_ack`. It returns whether the batch was non-empty;
`app.rs` retains the one `RendererFrameOwner::mark_dirty` decision.

**Tech Stack:** Rust, crossbeam-channel, tui-textarea, Cargo tests, Node
contract validators.

---

### Task 1: Spec Gate And RED Owner Tests

**Files:**

- Create: `docs/superpowers/specs/2026-08-16-tui-renderer-interaction-acks.md`
- Create: `docs/superpowers/plans/2026-08-16-tui-renderer-interaction-acks.md`
- Create: `crates/orca-tui/src/renderer_interaction_acks.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [x] Audit receiver construction, capacity/producer paths, renderer drain,
  single-ack reducer, downstream tests, contracts, roadmap, and exact counts.
- [x] Create isolated `codex/tui-renderer-interaction-acks` from clean local
  `main` and write this Proposed spec and plan before production edits.
- [x] Register `mod renderer_interaction_acks;` and add owner-level tests that
  import the absent `RendererInteractionAckOwner`.
- [x] Cover empty/disconnected drain, no-op activity, FIFO multi-ack drain,
  post-construction send, and failed-input restoration with real state.
- [x] Run `cargo test -p orca-tui renderer_interaction_acks --lib --locked --
  --test-threads=1`; require RED only because the production owner is absent.

### Task 2: Implement The Receiver Owner

**Files:**

- Modify: `crates/orca-tui/src/renderer_interaction_acks.rs`
- Modify: `crates/orca-tui/src/app.rs:250,311-324`

- [x] Add private `RendererInteractionAckOwner` retaining the existing
  `Receiver<InteractionResponseAck>`.
- [x] Implement non-blocking `try_iter` drain through the existing reducer,
  preserving FIFO, collaborators, all-current-items behavior, and a true
  result for every non-empty batch.
- [x] Construct the owner from the same `TuiAgentRuntime` receiver and replace
  only the raw app drain with `if owner.drain(...) { mark_dirty(); }`.
- [x] Keep receiver access, production/capacity, reducer behavior, frame dirty
  ownership, iteration order, and shutdown unchanged.
- [x] Run the owner suite GREEN, then focused `runtime_event_actions`,
  `action_dispatcher`, `agent_runtime`, and `renderer_frame` suites.

### Task 3: Close Contracts And Evidence

**Files:**

- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify the spec and this plan with measured evidence.

- [x] Add one `renderer_interaction_acks` entrypoint with exact app and owner
  anchors; increment the Rust mirror without broadening any baseline.
- [x] Add deletion self-tests for app construction/drain/dirty marking and
  owner receiver/try-iteration/reducer/activity paths while masks remain.
- [x] Regenerate manifest SHA-256 and update only its digest entry.
- [x] Update roadmap owner/count/next-boundary wording, exact source counts,
  implemented evidence, and completed plan checkboxes.
- [x] Run ordinary/test compiler gates, both validators/self-tests, formatter,
  digest equality, and diff check.

### Task 4: Full Verification, Review, Integration, And Cleanup

- [x] Run the pre-review full serial TUI and PTY gates: 1,136/1,136 and 6/6.
- [x] Stage the exact 11 slice files and run CodeRabbit over the complete
  staged diff; it reported zero findings.
- [x] Create one semantic commit: `refactor(tui): own renderer interaction
  acknowledgements`.
- [x] Rebase latest local `main` (already up to date); repeat owner 4/4,
  affected 40/40, validators/self-tests, full TUI 1,136/1,136, and PTY 6/6.
- [x] Fast-forward local `main`; repeat root owner 4/4, validators/self-tests,
  full TUI 1,136/1,136, and PTY 6/6.
- [x] Immediately remove only `.worktrees/tui-renderer-interaction-acks` and
  `codex/tui-renderer-interaction-acks`; preserve unrelated worktrees and
  record final evidence in the same semantic commit.
