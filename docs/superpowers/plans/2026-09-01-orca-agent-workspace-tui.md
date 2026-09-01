# Orca Agent Workspace TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn Orca's existing subagent event stream into a stable, default-visible activity dock and an interactive Agent Workspace.

**Architecture:** Keep runtime ownership unchanged: the TUI consumes `BackgroundTaskSummary` projections and never invents child state. Add a TUI-owned selection model over ordinary and workflow agents, use stable creation order for row identity, and route supported actions through the existing typed transcript and task-control messages.

**Tech Stack:** Rust, Ratatui, Crossterm, Orca typed runtime surface

**Spec:** `docs/superpowers/specs/2026-08-30-subagent-observability-user-trust-recovery.md`

## Global Constraints

- Preserve the existing dirty observability and cancellation work in the shared checkout.
- Do not add unfenced transcript reads or filesystem transcript fallbacks.
- Do not expose resume, retry, foreground, or follow-up controls without a typed runtime action.
- Keep the conversation activity dock bounded so fan-out cannot displace the transcript or composer.
- Keep rows stable while activity timestamps change.
- Do not commit from this shared dirty worktree unless the user explicitly asks.

---

### Task 1: Agent Workspace Selection Model

**Files:**
- Create: `crates/orca-tui/src/agent_workspace.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/workflow_panel.rs`

**Interfaces:**
- Consumes: `BackgroundTaskSummary`, `WorkflowAgentTaskSummary`
- Produces: `AgentWorkspaceState`, `AgentWorkspaceRow`, stable selection and summary counts

- [x] **Step 1: Write failing state tests**

  Add tests proving ordinary and workflow agents share one row model, rows stay in creation order when activity changes, selection stays on the same identity after refresh, and reset clears selection.

- [x] **Step 2: Run the focused tests and confirm RED**

  Run: `cargo test -p orca-tui --lib agent_workspace --locked`

  Expected: compilation/test failure because the workspace model does not exist.

- [x] **Step 3: Implement the minimal state model**

  Derive rows from the actor-owned task summaries, assign stable row identities, reconcile selection by identity, and expose only borrowed projected data.

- [x] **Step 4: Run the focused tests and confirm GREEN**

  Run: `cargo test -p orca-tui --lib agent_workspace --locked`

  Expected: all agent workspace state tests pass.

### Task 2: Interactive Agent Workspace Controls

**Files:**
- Create: `crates/orca-tui/src/agent_workspace_actions.rs`
- Modify: `crates/orca-tui/src/idle_key_actions.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/key_event_actions.rs`

**Interfaces:**
- Consumes: selected `AgentWorkspaceRow`
- Produces: `UserAction::ReadTaskTranscript` and `UserAction::StopTask` for supported ordinary subagents

- [x] **Step 1: Write failing action tests**

  Add tests for Up/Down selection, fenced Enter transcript, `s` stop for a live ordinary subagent, no fabricated action for workflow-only rows, mouse-wheel selection, and Esc close.

- [x] **Step 2: Run the focused tests and confirm RED**

  Run: `cargo test -p orca-tui --lib agent_workspace_actions --locked`

  Expected: failure because the Agents panel has no action handler.

- [x] **Step 3: Implement typed actions and navigation**

  Route panel keys before composer input, preserve typed transcript revision fencing, send stop through the existing control action, and close both task panels consistently with Esc.

- [x] **Step 4: Run the focused tests and confirm GREEN**

  Run: `cargo test -p orca-tui --lib agent_workspace_actions --locked`

  Expected: all action tests pass.

### Task 3: Stable Activity Dock And Workspace Renderer

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

**Interfaces:**
- Consumes: `AgentWorkspaceRow`, task lifecycle, activity, turn, usage, timestamps, continuation
- Produces: bounded conversation dock and selectable full-screen workspace

- [x] **Step 1: Write failing render tests**

  Add TestBackend assertions for a dock header and stable rows, an overflow affordance pointing to `/agents`, a selected workspace row, summary counts, live activity, elapsed/usage/turn metadata, recovery truth, contextual actions, and narrow-terminal clipping.

- [x] **Step 2: Run the focused tests and confirm RED**

  Run: `cargo test -p orca-tui --lib ui::tests::agent --locked`

  Expected: assertions fail against the current read-only panel and activity-only stack.

- [x] **Step 3: Implement the minimal renderer**

  Render a compact `Agents` dock in stable spawn order and a full workspace with status-first rows plus selected-agent detail. Use only existing theme tokens and display-width-safe truncation.

- [x] **Step 4: Run the focused tests and confirm GREEN**

  Run the exact new UI tests by name, then `cargo test -p orca-tui --lib ui::tests --locked`.

### Task 4: Cross-Surface Regression Verification

**Files:**
- No production files beyond Tasks 1-3

**Interfaces:**
- Consumes: completed TUI change
- Produces: verification evidence without changing runtime contracts

- [x] **Step 1: Run focused TUI and observability suites**

  Run the agent workspace/action/UI tests plus `subagent_observability_contract`.

- [x] **Step 2: Run compilation and formatting gates**

  Run `cargo check -p orca-tui --all-targets --locked`, `cargo fmt --all -- --check`, and `git diff --check`.

- [x] **Step 3: Exercise a terminal-sized render matrix**

  Verify normal and narrow Ratatui terminal fixtures cover multiple live agents, attention state, completion, cancellation, and overflow without transcript/composer displacement.

- [x] **Step 4: Review the final diff against the request**

  Confirm the result combines Claude Code's default visibility, Grok's stable multi-row management, Codex's inspectable child identity, and Orca's durable lifecycle/recovery information without copying unsupported controls.
