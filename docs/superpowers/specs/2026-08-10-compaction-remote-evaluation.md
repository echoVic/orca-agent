# Compaction Completion and Remote Evaluation

## Problem and evidence

`RuntimeCompactionPolicy` and `RuntimeCompactionStep` already own soft/hard
pressure decisions, prompt-too-long retry, cancellation, remote-summary
fallback, history persistence, and ordered lifecycle events. The remaining
roadmap item is completion evidence: the remote path must be exercised through
the production context compactor and observed after a real provider wait.

## User value

Long DeepSeek sessions should retain a usable recent request and durable summary
facts instead of silently truncating context. A real API smoke makes the cost
and latency boundary observable without adding a second compaction loop.

## Scope and non-goals

This slice adds a credential-gated provider example and an evidence report. It
does not change compaction policy, summary prompts, API contracts, persistence
formats, or cancellation semantics. It does not introduce a background remote
worker; the existing cancellation-aware synchronous wait remains the owner.

## Acceptance

1. Runtime compaction focused tests pass, including event ordering, persistence,
   cancellation, retry, and recovery checks.
2. Provider context focused tests pass, including remote-summary fallback and
   in-flight cancellation.
3. The real API harness crosses the soft line, returns `RemoteSummary`, retains
   the current request, records a non-empty summary baseline, and lowers wire
   pressure. Missing credentials produce a successful skip.
4. The report records exact commands, observed output, and the boundary that
   remote work remains synchronously awaited and cancellation-owned.

## Compatibility and rollback

The example is additive and has no wire or persistence impact. Removing the
example and report fully reverts this slice.
