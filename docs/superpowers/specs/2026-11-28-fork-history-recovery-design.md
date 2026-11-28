# Fork Children Recover from Damaged History Files

## Intent

A fork child's inherited history is stored once per prefix. Two kinds of file hold it:
- a content-addressed prefix file;
- a small per-child association file naming the prefix.

Two review findings from PR #196 remain, and both make one damaged file affect more than its own child.

## Goals

1. **A damaged association affects only its own child.**
   - Today `read_record` resolves fork history while the task registry reconciles every indexed record at startup. A truncated or otherwise unreadable association therefore makes `TaskRegistry::new_persistent` fail for the whole session.
   - After the fix, reconciliation validates and projects continuation records without resolving fork history.
   - Fork history is resolved when that child is created, prepared or resumed. A damaged association or prefix fails only that child, with a clear error, and every other task opens and runs.
2. **A failed fork creation publishes nothing.**
   - Today `create_with_fork` writes the continuation record and its task projection before `prepare_record` finds that the prefix file is unavailable. The ID is then bound to a `Created` record with a `Prepared` attempt, and a fresh retry with the same ID is refused.
   - After the fix, the prefix is loaded and validated before `create_record`. A failure leaves no record, no projection and no association.
   - The loaded prefix is carried into the result, so it is not looked up a second time.

## Must keep

- Conflicting digests are still refused: the inline reference and the association naming different prefixes.
- Association-only recovery still works for records an older release rewrote without the inline reference.
- The fixes from PR #196 still hold: a missing association falls back to the inline reference, a fresh creation removes an orphan association, and a missing prefix leaves a resume retryable.
- Nothing new is persisted, and the file formats do not change.

## Non-goals

- No garbage collection of prefix files.
- No change to how fork prefixes are captured or redacted.

## Approach

Split fork-history resolution out of `read_record` into a step that runs only when a child is prepared. That covers creation, `prepare_record` and resume preparation. Reconciliation then needs only the record itself.
