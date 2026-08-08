# Auto-Memory Governance Spec

## Problem and Evidence

Final-response auto-memory is invoked from
`crates/orca-runtime/src/provider_turn.rs` after the assistant response is
recorded. The call currently creates a fresh `CancelToken` through
`extract_project_memory`, so cancellation of the owning turn cannot stop the
auxiliary provider request. `crates/orca-runtime/src/memory.rs` also documents
`append_note` as requiring single-session usage and appends directly without a
cross-process lock. Concurrent TUI/runtime sessions can therefore race while
writing the same project memory file.

## User and Architecture Value

TUI shutdown, interrupt, and operation cancellation must stop auxiliary memory
work owned by the same turn. Project memory writes must remain one coherent
append-only fact source when multiple runtime sessions finish at once. This
slice strengthens the existing runtime and memory boundaries without adding a
second worker, event stream, or persistence format.

## Scope

In scope:

- Pass the active runtime turn cancellation token into final-response memory
  extraction.
- Serialize each memory-file append with `orca_platform::fs::ExclusiveFileLock`
  on a sibling `.lock` file. Lock acquisition retries with short sleeps,
  checks cancellation, and fails after a five-second ceiling so a writer never
  waits forever.
- Add behavior tests for cancellation before persistence, caller-level
  `auto_memory` wiring, and concurrent append serialization.

Out of scope:

- Creating a new background task or supervisor; the turn remains the owner of
  the extraction call in this slice.
- Changing `/remember`, memory file paths, bullet format, prompt content,
  provider configuration, CLI/TUI/server protocol, or transcript schema.
- Deduplicating model-generated notes or redesigning memory retrieval.

## Lifecycle and Failure Semantics

- Normal final response: extraction uses the active turn token, and a valid
  non-`NOTHING` response appends one bullet while holding the file lock.
- Cancellation before or during provider streaming: extraction returns
  `Ok(None)` and performs no write; the owning turn retains its existing
  cancellation terminal. If cancellation races after the final append begins,
  that already-started append may complete, just like any filesystem write.
- Lock contention: the memory module retries for at most five seconds,
  checking cancellation between attempts. An uncancelled writer normally waits
  until the prior append releases the lock; an overlong holder returns an
  extraction error instead of blocking indefinitely.
- Provider error, empty response, or `NOTHING`: no memory write and no new
  error event, preserving current extraction behavior.
- Lock/filesystem failure: return the existing extraction error to the caller;
  the turn emits its existing `memory extraction failed` error event.
- Process exit during a write: the OS releases the lock; append-only recovery
  remains governed by the existing filesystem semantics and no migration is
  needed.

## Ownership and Boundaries

`RuntimeStepCapabilitySnapshot::cancel` is the authoritative turn cancellation
source. `RuntimeProviderResponseStep` passes it to the memory module. The
memory module owns prompt formatting, provider invocation, lock acquisition,
and the single append operation. `ExclusiveFileLock` owns cross-process
serialization. No detached worker, resettable token, parallel writer, or second
memory store is introduced.

## Compatibility

The public `/remember` API, project hash paths, `memory.md` bullet format,
provider request shape, CLI/TUI/server payloads, and persisted transcript schema
remain unchanged. The only observable change is that cancellation observed
before persistence prevents an automatic note from being persisted, and
concurrent writers serialize their existing append records.

## Acceptance Criteria

1. A RED test demonstrates that the final-response path honors an already
   cancelled turn token and leaves the memory file unchanged.
2. A deterministic lock-contention test proves an append waits for the shared
   lock, and a concurrent append test proves every complete bullet is present
   exactly once with no interleaved records.
3. The memory unit suite passes, including existing provider-config and manual
   memory behavior.
4. Runtime provider-turn tests pass with the active cancellation token wired
   through, and formatter/diff checks are clean.
5. The change contains no new worker, protocol type, persistence schema, or
   duplicate memory fact source.

## Verification Commands

```bash
cargo test -p orca-runtime memory --lib -- --test-threads=1
cargo test -p orca-runtime provider_turn --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

## Migration and Removal

This is a bounded correctness slice. The direct uncancellable final-response
call and unlocked append are removed in the same commit. A later host-level
memory task migration may move extraction off the turn only if it preserves the
same cancellation, join, and lock ownership contract; it is not required here.

## Final Evidence

- RED was confirmed before implementation: the new final-response test failed
  to compile because the helper accepted no cancellation token, and the lock
  test had no shared lock-path owner.
- GREEN memory evidence: 11 filtered runtime tests passed, including cancelled
  extraction, cancellation during lock contention, one blocked writer, and 16
  concurrent complete records.
- GREEN provider-turn evidence: 17 filtered runtime tests passed with the active
  turn cancellation token wired into the final-response path; the refreshed
  filter now passes 18 tests, including the `auto_memory=true` caller branch.
