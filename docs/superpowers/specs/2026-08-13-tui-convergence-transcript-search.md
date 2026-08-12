# TUI Convergence Slice 9: Transcript Search Orchestration Extraction

## Status

Proposed for `codex/tui-convergence`, based on `main` at `5899e9ac1`.

## Problem And Evidence

`crates/orca-tui/src/types.rs` (9,243 lines) still mixes the transcript
search orchestration with the AppState reducer. The six search methods
(types.rs:2174-2230: `open_transcript_search`, `close_transcript_search`,
`replace_transcript_search_query`, `refresh_transcript_search`,
`search_next`, `search_previous`) are a coherent, bounded category that
bridges `TranscriptSearchState` (already owned by
`crates/orca-tui/src/transcript_search.rs`) with the render cache and
scroll state. Strong behavioral tests already cover the category (app.rs
transcript-search tests: open/replace/refresh/next/previous, esc-close,
stale-query refresh).

Classification: architecture boundary (module ownership), no behavior
change.

## Scope

Move the six `impl AppState` methods into
`crates/orca-tui/src/transcript_search.rs` as an additional `impl AppState`
block in that module. All six are already `pub(crate)`, so no visibility
change is needed. `types.rs` keeps the fields and the `update` reducer;
the methods move verbatim. The new impl block needs `use` of `AppState`,
`AppStatus`, `PanelMode` (and `std::time::Instant` if not already used by
the module).

No logic edits. No baseline drift expected: the runtime-surface-contract
inventories only record `TranscriptSearchState` methods that stay in the
module (verified: no baseline site references the six AppState methods or
`cache.search` / `refresh_with` / `reveal_offset`); the validators still
run as the acceptance gate.

## Non-Goals

- No move of `TranscriptSearchState`/`SearchQuery` (they already live in
  transcript_search.rs), the `update` reducer, scroll/status fields, or
  the render cache.
- No CLI/TUI-flow/server/JSONL/persistence changes.
- No source-line-count assertions; the existing tests are the oracle.

## Ownership

`transcript_search.rs` owns the search state machine AND the AppState
orchestration that feeds it; `types.rs` owns the fields and the reducer
that dispatches into it. `global_actions.rs` / `input_event_actions.rs` /
`key_event_actions.rs` keep owning the key dispatch.

## Normal / Failure Semantics

Unchanged: `open_transcript_search` only opens in Conversation panel with
Idle/Running/WaitingUserInput status; `search_next`/`search_previous`
refresh first and leave scroll untouched when no match.

## Acceptance

1. The six methods live in `transcript_search.rs` (impl AppState);
   types.rs no longer defines them; compile clean.
2. Behavioral oracle unchanged and green:
   - `cargo test -p orca-tui --lib --locked` (1034 tests)
   - `cargo test --test tui_pty_contract --locked -- --test-threads=1` (6 tests)
3. `cargo fmt --all -- --check` and `git diff --check` clean.
4. Both surface-contract validators green (baseline-maintenance step):
   - `node --test scripts/test-validate-runtime-surface-contract.mjs`
   - `node --test scripts/test-validate-windows-platform-boundaries.mjs`
5. Diff review: relocation only (imports in the new impl block are the
   only additions).

## Rollback

Single revertible commit; no persisted state.

## Migration

No temporary state; the old paths (methods in types.rs) are removed in the
same commit, not kept as wrappers.
