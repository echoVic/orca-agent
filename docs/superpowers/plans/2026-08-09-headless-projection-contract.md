# Headless Terminal Projection Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove that streamed and persisted headless tool terminals are the same ordered projection.

**Architecture:** Extend the existing real-binary headless contract only. Extract the established terminal fields from the JSONL event sink and SessionWriter output, then compare ordered tuples; no runtime or persistence owner changes are needed.

**Tech Stack:** Rust, Cargo integration contracts, JSONL, serde_json.

---

### Task 1: Add the RED projection assertion

**Files:**
- Modify: `tests/agent_loop_contract.rs:115-164`

- [x] **Step 1: Add an ordered projection comparison.**

Keep the existing count and fixture-id assertions. Build one vector from each
projection using `id`, `status`, `kind`, and `exit_code`, then assert equality
with a failure message that identifies the two ordered vectors.

- [x] **Step 2: Run the focused test and confirm RED.**

First add the comparison with one deliberately wrong expected value:
`assert_eq!(completed_tools[0]["payload"]["exit_code"], 1)`. Run the exact focused command
and confirm it fails at that assertion while the process still exits with code
4 and emits the expected 128 records. Replace only that deliberate value with
the persisted projection comparison before proceeding to GREEN. RED output was
`left: Number(0), right: 1` at the deliberate assertion.

### Task 2: Implement the minimal GREEN contract

**Files:**
- Modify: `tests/agent_loop_contract.rs:115-164`

- [x] **Step 1: Extract existing terminal fields from both projections.**

Use `as_str()` and `as_i64()` with explicit `expect` messages for required
fixture fields. Map each event with
`(payload.id, payload.status, payload.kind, payload.exit_code)` and each
persisted record with
`(message.tool_call_id, message.status, message.kind, message.exit_code)`;
preserve order from both vectors and compare the resulting tuples directly.

- [x] **Step 2: Run focused and neighboring contracts.**

Run the exact boundary test, then the serial `agent_loop_contract` and
`runtime_lifecycle_contract` suites. The existing 128-turn assertions and
runtime behavior must remain unchanged. The focused test passed in 48.62s;
`agent_loop_contract` passed 4/4 and `runtime_lifecycle_contract` passed 54/54.

### Task 3: Document and review

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-09-headless-projection-contract.md`
- Modify: `docs/superpowers/plans/2026-08-09-headless-projection-contract.md`

- [x] **Step 1: Record the projection evidence.**

Document the ordered event/transcript comparison and fresh test counts in the
roadmap, Spec, and Plan without claiming a runtime change.

- [x] **Step 2: Review for scope drift.**

Confirm the diff changes only the contract, documentation, and roadmap; there
  is no duplicate writer, event parser, lifecycle owner, or protocol change.
Review confirmed the diff is limited to the contract, roadmap, Spec, and Plan.

### Task 4: Deliver the semantic slice

- [x] **Step 1: Run formatter and diff checks.**

```bash
cargo fmt --all -- --check
git diff --check
```

- [x] **Step 2: Commit, rebase, re-verify, and integrate.**

Commit the contract and evidence as one semantic slice, rebase onto the latest
`origin/main`, rerun the focused and affected gates, then fast-forward the main
checkout. (Delivery evidence is recorded after the commit and rebase.)
Post-rebase evidence: the focused boundary contract passed 1/1 in 49.35s,
`agent_loop_contract` passed 4/4 in 52.30s, `runtime_lifecycle_contract` passed
54/54 in 2.77s, and the branch rebased cleanly on `origin/main`.
