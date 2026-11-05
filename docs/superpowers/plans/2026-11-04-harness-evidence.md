# Harness evidence implementation plan

**Goal:** Implement the first five accepted recommendations from the monthly report.
**Architecture:** Extend existing event, request, evaluation and image routing paths. Keep completion proof, file observations and task outcomes separate. No new endpoint pool or compression policy.
**Tech stack:** Existing Rust workspace and Python capability harness.
**Spec:** docs/reports/2026-11-04-wiki-orca-recommendations.md, sections 1–5.

## Constraints

All edits use shell/Python, never apply_patch. Worktree base 5f1dafd; do not copy unrelated branding changes. Explicit no-history remains respected. New event fields are additive. No live paid model evaluation without a bounded request already authorized. Do not commit or push this batch automatically.

## Review focus

Resume metadata must describe durable history rather than merely an allocated ID. Existing dirty files and concurrent changes cannot be attributed to this run. Retry-After exceeding deadline must never lead to an early retry. Tool observations are untrusted, scoped to their original call and snapshot. Missing provider metadata stays unknown.

## Tasks

### 1. Headless evidence (main worker)
- [x] RED: runtime event/exec contracts exercise resumable true/false and reason, current unresolved child blockers, shell file changes including dirty baseline and restoration.
- [x] Implement additive completion context in event schema/controller; collect current task-tree blockers; bound workspace observation using filesystem directory handles and content hashes with unknown attribution; keep writes_applied unchanged.
- [x] GREEN: core event tests plus runtime/controller and headless JSONL integration tests.

### 2. Shared provider retry (parallel worker)
- [x] RED: integer/date Retry-After, same endpoint/credential cooling across requests, cancellation, bounded nested retries.
- [x] Extend existing HTTP request paths and runtime retry integration with shared scope and logical attempt budget, no plaintext credential logging.
- [x] GREEN: provider HTTP tests and runtime retry tests.

### 3. Evaluation evidence (parallel worker)
- [x] RED: actual execution model configuration, unknown response fingerprint, fixture/verifier hashes, hidden-constraint evidence tracing and repeated-run aggregation.
- [x] Extend scripts/eval/capability only; offline fixtures verify constraints and final state independently.
- [x] GREEN: capability Python tests and dry-run commands.

### 4. Tool image sidecar (parallel worker)
- [x] RED: non-Flash tool screenshot description, dedupe per observation/query, failure/cancel, no system-trusted observation.
- [x] Extend image_routing only and focused tests; preserve original tool result pairing, provenance and dimensions where available.
- [x] GREEN: image routing/provider image compatibility contracts.

### 5. Compression observation (parallel worker)
- [x] RED: before/after local token estimates, summary request usage/cost and duration emitted; local prefix estimate distinguished from service cache usage.
- [x] Extend compaction events with optional measurement; do not change budget thresholds or policy.
- [x] GREEN: focused compaction tests.

### 6. Integration
- [x] Review each worker diff and RED/GREEN evidence; add missing cross-component checks.
- [x] cargo fmt --check, targeted checks, Python tests, full workspace tests; report environment-specific failures by name. Full CI run 4798 passed / 1 proxy timeout; unchanged serial recheck passed. TUI lib 1848 passed (1 leaky) and PTY 40 passed; details in delivery report.
- [x] Verify original source hashes preserved and final worktree diff clean; deliver implementation and test evidence. Original 29 file hashes unchanged, final code snapshot unchanged, owned manifest 56 files, staging empty, HEAD remains 5f1dafd.
