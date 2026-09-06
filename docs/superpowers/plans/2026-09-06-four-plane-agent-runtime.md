# Four-Plane Agent Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (or subagent-driven-development) to implement this plan task-by-task.

**Goal:** Replace mixed subagent task/transcript state with independent child threads, one typed agent event registry, a persistent dock, and decoupled execution profiles.

**Architecture:** Reuse the existing `AgentEventEnvelope` and `AgentRegistry` as the canonical agent journal. Keep surface snapshots for thread transcripts and task controls, but stop materializing child activity into root `ChatMessage` values. Route focus and input through runtime-owned child handles.

**Tech Stack:** Rust 2024, Tokio, crossbeam channels, ratatui, serde JSONL, GitHub Actions, npm.

**Spec:** `docs/superpowers/specs/2026-09-06-four-plane-agent-runtime-design.md`

## Global Constraints

- Root transcript must contain only one delegation announcement for a batch.
- Child tool activity and output must be rendered only by the focused child thread.
- Dock remains visible in root and child views; `/agents` is a detail view.
- Every canonical event is identity-bound, ordered, digest-validated, and dead-lettered once on corruption.
- Approval policy and execution profile are independent; children may only narrow profiles.
- No release/npm publication or cache cleanup until the complete locked gate and real TUI proof pass.

### Task 1: Make Root Transcript Thread-Only

**Files:**
- Modify: `crates/orca-tui/src/transcript_state.rs`
- Modify: `crates/orca-tui/src/state_reducer.rs`
- Modify: `crates/orca-tui/src/workflow_panel.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Test: `crates/orca-tui/src/state_integration_tests.rs`

- [ ] Remove `ChatMessage::Subagent` and the `sync_subagent_transcript_messages` materializer.
- [ ] Preserve one system delegation announcement keyed by `batch_id`; never append child activity/output to root messages.
- [ ] Add a regression asserting a root projection contains the announcement but no child message/tool lines.
- [ ] Keep focused child history in the existing typed surface projection path.
- [ ] Run the TUI library tests and commit.

### Task 2: Wire Canonical Agent Registry To Dock

**Files:**
- Modify: `crates/orca-tui/src/protocol.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/hosted_controller.rs`
- Modify: `crates/orca-runtime/src/agent_registry.rs`
- Test: `crates/orca-tui/src/idle_key_actions.rs`

- [ ] Add a typed `AgentRegistryUpdated(AgentRegistrySnapshot)` event and state projection.
- [ ] Poll/forward registry snapshots from the hosted controller without reading registry files in TUI code.
- [ ] Derive dock rows from registry summaries with parent/thread identity and bounded activity detail.
- [ ] Keep Shift navigation, Enter focus, and Esc return unchanged at the action boundary.
- [ ] Add tests for one-frame Main plus four children, overflow, and focus selection.
- [ ] Run focused TUI integration tests and commit.

### Task 3: Remove Agent Presentation Event Dual Track

**Files:**
- Modify: `crates/orca-core/src/event_schema.rs`
- Modify: `crates/orca-tui/src/protocol.rs`
- Modify: `crates/orca-tui/src/hosted_session.rs`
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/src/provider_turn.rs`
- Modify: `crates/orca-runtime/src/tool_turn.rs`
- Test: `tests/subagent_contract.rs`

- [ ] Stop emitting `WorkflowTasksUpdated` and `TaskStatusUpdated` for ordinary agents.
- [ ] Emit canonical registry envelopes for Spawned, Activity, OutputDelta, PermissionRequested, and terminal events.
- [ ] Keep workflow-owned task events only for workflow-specific projections.
- [ ] Assert no duplicate delegated-task stream is emitted for one child.
- [ ] Run runtime and contract tests and commit.

### Task 4: Decouple Execution Profile From Approval

**Files:**
- Modify: `crates/orca-core/src/capability.rs`
- Modify: `crates/orca-core/src/execution_broker.rs`
- Modify: `crates/orca-tools/src/process.rs`
- Modify: `crates/orca-tui/src/protocol.rs`
- Modify: `crates/orca-tui/src/state_reducer.rs`
- Test: `crates/orca-core/src/execution_broker.rs`

- [ ] Introduce explicit `ApprovalPolicy` and `ExecutionProfile` resolution with parent-ceiling intersection.
- [ ] Return a typed sandbox availability decision for Workspace/ReadOnly when enforcement is unavailable.
- [ ] Ensure TrustedHost is opt-in and RemoteSandbox never falls through to local spawn.
- [ ] Render one actionable availability prompt and deduplicate it by request/profile digest.
- [ ] Run capability, broker, and TUI interaction tests and commit.

### Task 5: Harden Single-Writer Event Journal

**Files:**
- Modify: `crates/orca-core/src/agent_event.rs`
- Modify: `crates/orca-runtime/src/agent_registry.rs`
- Modify: `crates/orca-runtime/src/subagent_event_relay.rs`
- Test: `crates/orca-runtime/src/agent_registry.rs`
- Test: `tests/subagent_contract.rs`

- [ ] Validate event identity, attempt, sequence, digest, and schema before append.
- [ ] Dead-letter malformed middle/final frames once while advancing the attempt cursor.
- [ ] Make terminal status sticky and reject late activity.
- [ ] Add restart, duplicate, corrupt-frame, and poisoned-sequence tests.
- [ ] Run the complete focused runtime contract set and commit.

### Task 6: Real TUI Verification And Documentation

**Files:**
- Modify: `tests/tui_pty_contract.rs`
- Create: `docs/verification/four-plane-agent-runtime-tui.txt`
- Modify: `docs/releases/vX.Y.Z.md`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`

- [ ] Run the mock-provider PTY flow: launch four children, verify one announcement, visible two-line dock, Shift focus, Enter child transcript, Esc return.
- [ ] Repeat through cmux Computer Use and save a screenshot path plus terminal transcript evidence.
- [ ] Run fmt, clippy, locked nextest, validators, site build, and npm staging checks.
- [ ] Bump a new patch version only after all gates pass; tag and publish release/npm.
- [ ] Verify GitHub Release and npm package, then remove only validated Rust build caches and report reclaimed space.
