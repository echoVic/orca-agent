# Automatic Memory Cancellation File Contract

## Problem and Evidence

`append_note_with_cancel` opens a missing `memory.md` with `create(true)`
before its final cancellation check. Cancellation after that check but before
the first write can leave a newly-created empty target file, despite reporting
that no note was persisted. The existing lock remains the correct ownership
boundary; the target creation order is the defect.

## User and Architecture Value

Cancelled automatic memory extraction must leave the project memory target
unchanged. The memory module remains the sole writer, and its existing
sidecar lock continues to serialize all append and cleanup decisions.

## Scope and Non-Goals

In scope:

- stage a first note in a same-directory temporary file and publish it only
  after cancellation checks;
- roll back an append to an existing file when cancellation is observed after
  the write but before completion;
- add focused tests for missing-target and pre-existing-target cancellation.

Out of scope:

- changing lock ownership, retry policy, memory format, provider extraction, or
  manual `/remember` semantics;
- promising impossible cancellation after a successful committed return.

## Semantics and Ownership

- The sidecar `ExclusiveFileLock` is held through staging, append, flush, and
  any rollback/publish operation.
- A missing target is published with an atomic same-directory rename only after
  the staged note is complete and cancellation has not been observed.
- An existing target records its original length and truncates back to that
  length if cancellation is observed after the append.
- Temporary files are removed on cancellation or staging/publish failure.

## Compatibility

Successful writes retain the existing `- note` line format and append order.
Existing memory files are not replaced. No public API, persistence schema, or
provider behavior changes.

## Acceptance Criteria

1. A cancelled append to a missing target leaves no `memory.md` and no temp
   artifact.
2. A cancelled append to an existing target leaves its original bytes intact.
3. Successful append and lock contention tests continue to pass.
4. Focused memory/provider-turn tests, formatting, and diff checks pass.

## Verification

```bash
cargo test -p orca-runtime memory --lib -- --test-threads=1
cargo test -p orca-runtime provider_turn --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

## Migration and Rollback

No migration. Reverting restores the previous append implementation without
changing existing memory files.
