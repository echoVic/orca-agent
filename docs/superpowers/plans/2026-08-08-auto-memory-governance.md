# Auto-Memory Governance Implementation Plan

> Historical plan. Superseded on 2026-08-13 by
> `../specs/2026-08-13-auto-memory-v2.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make final-response auto-memory cancellation-aware and serialize
concurrent append-only writes through the existing platform lock.

**Architecture:** Keep the owning runtime turn as the extraction owner. Thread
its existing `CancelToken` into `memory.rs`, and make that module the only
writer of the project memory file under an `ExclusiveFileLock`.

**Tech Stack:** Rust, `CancelToken`, `orca_platform::fs::ExclusiveFileLock`,
runtime unit tests.

---

### Task 1: Spec Gate and RED tests

**Files:**
- Add: `docs/superpowers/specs/2026-08-08-auto-memory-governance.md`
- Add: `docs/superpowers/plans/2026-08-08-auto-memory-governance.md`
- Modify: `crates/orca-runtime/src/memory.rs`

- [x] **Step 1: Record current ownership and acceptance boundaries.**

- [x] **Step 2: Add a cancellation RED test for the final-response helper.**

Exercise the helper with an already-cancelled `CancelToken` and assert the
project memory file remains absent and no memory error event is emitted.

- [x] **Step 3: Add a concurrent append RED test.**

Start multiple threads appending unique notes to one temporary memory file and
assert the complete bullet records can be recovered without partial lines or
lost writes. Pair this with the deterministic blocked-lock test, which fails
without the shared lock owner.

### Task 2: Implement the governed memory path

**Files:**
- Modify: `crates/orca-runtime/src/memory.rs`
- Modify: `crates/orca-runtime/src/provider_turn.rs`

- [x] **Step 1: Thread the active turn token into extraction.**

Pass `RuntimeStepCapabilitySnapshot::cancel` to a cancel-aware final-response
helper and preserve existing no-note/provider-error behavior.

- [x] **Step 2: Serialize writes with a sibling lock.**

Acquire `memory.md.lock` (or the corresponding file sibling) with
`ExclusiveFileLock::try_acquire`, retrying for at most five seconds while
checking cancellation. Hold the lock through the append and release it before
returning.

- [x] **Step 3: Run focused GREEN tests and refactor only within the boundary.**

Confirm cancellation never writes and concurrent writers retain all complete
records; keep manual `/remember` behavior compatible.

### Task 3: Documentation and gates

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-08-auto-memory-governance.md`
- Modify: `docs/superpowers/plans/2026-08-08-auto-memory-governance.md`

- [x] **Step 1: Record the new cancellation and lock contract.**

- [x] **Step 2: Run required runtime, format, and diff gates.**

```bash
cargo test -p orca-runtime memory --lib -- --test-threads=1
cargo test -p orca-runtime provider_turn --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Evidence: the focused memory filter passed 11 tests, the provider-turn filter
passed 18 tests, the complete serial runtime library gate passed 1037 tests,
and formatter and diff checks passed. Existing non-deny dead-code warnings
remain unchanged.

### Task 4: Review and delivery

- [x] **Step 1: Review the diff for duplicate owners, detached work, and
  compatibility drift.**

- [x] **Step 2: Create one semantic commit.**

```bash
git add crates/orca-runtime/src/memory.rs crates/orca-runtime/src/provider_turn.rs docs/production-roadmap.md docs/superpowers/specs/2026-08-08-auto-memory-governance.md docs/superpowers/plans/2026-08-08-auto-memory-governance.md
git commit -m "fix(memory): govern automatic project writes"
```

- [x] **Step 3: Fetch/rebase `origin/main` and rerun affected gates.**

The feature branch must be rebased before delivery; post-rebase focused tests,
runtime lifecycle coverage, formatting, and diff checks are fresh evidence.

Post-commit evidence: `origin/main` was current and the rebase was clean;
post-rebase memory passed 11 tests, provider-turn passed 18 tests, the full
serial runtime library gate passed 1037 tests in 159.81s, and formatter/diff
checks passed. Clippy passed with only existing repository warnings.
