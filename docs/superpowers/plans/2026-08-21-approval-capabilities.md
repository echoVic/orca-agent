# Approval Capabilities Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Tasks 1–3 implemented in `refactor: persist reusable shell permission capabilities` (5640858b0) and verified by the workspace gate. Task 4 item 1 was already satisfied by the centralized permission request policy (`refactor(runtime): centralize permission request policy`, 6d1e6d48d); Task 4 items 2–3 remain future hardening and are intentionally left unchecked.

**Goal:** Make permission grants explicit, scoped, reusable, and durable so a sandbox denial cannot trigger repeated approval requests or silently widen authority.

**Architecture:** Runtime permission responses are normalized into a capability set with `Turn` or `Session` scope. The turn overlay carries only active capabilities; session capabilities are materialized into thread settings and restored into every new turn. Shell unsandboxed authority is a first-class capability, not an implicit retry mode. All retries are bound to the exact requested capability and denied when the diagnostic is not safely attributable.

**Tech Stack:** Rust workspace, `orca-runtime` runtime/surface layers, JSONL session metadata, existing Seatbelt/Linux sandbox adapters, unit and integration tests.

---

### Task 1: Add first-class shell capability to runtime permission state

**Files:**
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Test: `crates/orca-runtime/src/runtime_permission.rs`

- [x] Add `unsandboxed_shell: bool` to the turn overlay and its delta.
- [x] Make permission merging, delta calculation, and delta application preserve the capability.
- [x] Add tests proving the capability is reusable in a turn and cannot be introduced by an unrelated delta.

### Task 2: Make bash consume capabilities before prompting

**Files:**
- Modify: `crates/orca-runtime/src/runtime_bash.rs`
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Test: `crates/orca-runtime/src/runtime_bash.rs`

- [x] Skip the unsandboxed permission request when the current overlay already grants it.
- [x] Record the grant in the overlay only after an allow response.
- [x] Add a regression test for two pathless denials in one turn requiring one request.

### Task 3: Persist session shell capability through surface and JSONL metadata

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/operation.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/interaction.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/server/processors/permission.rs`
- Modify: `crates/orca-runtime/src/server/surface_adapter.rs`
- Modify: `crates/orca-runtime/src/thread_store/types.rs`
- Modify: `crates/orca-runtime/src/thread_store/local.rs`
- Test: affected surface and server tests

- [x] Add session settings and retry capsule fields for shell capability.
- [x] Validate session grants as a subset of the requested profile.
- [x] Persist and restore shell capability for recorded sessions; keep ephemeral sessions runtime-only.

### Task 4: Enforce denial attribution and event semantics

**Files:**
- Modify: `crates/orca-runtime/src/sandbox_denial.rs`
- Modify: `crates/orca-runtime/src/runtime_bash.rs`
- Modify: `crates/orca-runtime/src/tool_execution.rs`
- Test: denial and approval event tests

- [x] Classify pathless denials with an explicit retry fingerprint.
- [ ] Bound repeated identical retries within a turn and fail closed after the bound.
- [ ] Keep automatic authorization in the audit stream but stop projecting it as a user approval interaction.

### Task 5: Verification

- [ ] Run focused `orca-runtime` tests for permission, bash, server, and surface modules.
- [ ] Run `cargo fmt --check` and `git diff --check`.
- [ ] Run the affected package test gate and record any environment-gated sandbox tests that cannot run.
