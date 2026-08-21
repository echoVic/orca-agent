# Approval Capabilities Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make permission grants explicit, scoped, reusable, and durable so a sandbox denial cannot trigger repeated approval requests or silently widen authority.

**Architecture:** Runtime permission responses are normalized into a capability set with `Turn` or `Session` scope. The turn overlay carries only active capabilities; session capabilities are materialized into thread settings and restored into every new turn. Shell unsandboxed authority is a first-class capability, not an implicit retry mode. All retries are bound to the exact requested capability and denied when the diagnostic is not safely attributable.

**Tech Stack:** Rust workspace, `orca-runtime` runtime/surface layers, JSONL session metadata, existing Seatbelt/Linux sandbox adapters, unit and integration tests.

---

### Task 1: Add first-class shell capability to runtime permission state

**Files:**
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Test: `crates/orca-runtime/src/runtime_permission.rs`

- [ ] Add `unsandboxed_shell: bool` to the turn overlay and its delta.
- [ ] Make permission merging, delta calculation, and delta application preserve the capability.
- [ ] Add tests proving the capability is reusable in a turn and cannot be introduced by an unrelated delta.

### Task 2: Make bash consume capabilities before prompting

**Files:**
- Modify: `crates/orca-runtime/src/runtime_bash.rs`
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Test: `crates/orca-runtime/src/runtime_bash.rs`

- [ ] Skip the unsandboxed permission request when the current overlay already grants it.
- [ ] Record the grant in the overlay only after an allow response.
- [ ] Add a regression test for two pathless denials in one turn requiring one request.

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

- [ ] Add session settings and retry capsule fields for shell capability.
- [ ] Validate session grants as a subset of the requested profile.
- [ ] Persist and restore shell capability for recorded sessions; keep ephemeral sessions runtime-only.

### Task 4: Enforce denial attribution and event semantics

**Files:**
- Modify: `crates/orca-runtime/src/sandbox_denial.rs`
- Modify: `crates/orca-runtime/src/runtime_bash.rs`
- Modify: `crates/orca-runtime/src/tool_execution.rs`
- Test: denial and approval event tests

- [ ] Classify pathless denials with an explicit retry fingerprint.
- [ ] Bound repeated identical retries within a turn and fail closed after the bound.
- [ ] Keep automatic authorization in the audit stream but stop projecting it as a user approval interaction.

### Task 5: Verification

- [ ] Run focused `orca-runtime` tests for permission, bash, server, and surface modules.
- [ ] Run `cargo fmt --check` and `git diff --check`.
- [ ] Run the affected package test gate and record any environment-gated sandbox tests that cannot run.
