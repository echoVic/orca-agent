# TUI Convergence Slice 10: Session Picker State Extraction

## Status

Proposed for `codex/tui-convergence`, based on `main` at `0de920810`.

## Problem And Evidence

`crates/orca-tui/src/types.rs` (9,185 lines) still mixes the saved-session
picker state machine with the AppState reducer. The picker category
(types.rs:2443-2544) is a coherent ownership unit:

- `filtered_session_indices` (query → filtered index set, with its doc
  comment),
- `select_previous_session`, `select_next_session`,
  `select_session_page_up`, `select_session_page_down`,
  `select_first_session`, `select_last_session` (selection navigation),
- `session_query_push`, `session_query_pop` (query editing, with the
  first-match reset invariant),
- `reset_session_selection_to_first_match` (private helper, used only by
  the query editors above),
- `selected_session_id` (selection resolution).

Key dispatch already lives in `session_picker_actions.rs`; the state
category has no module of its own. Behavioral tests cover the category
(app.rs session-picker tests plus `session_picker_actions.rs` dispatch
tests); external callers (`input_event_actions.rs:250`,
`session_picker_actions.rs:438/445`) use the public methods only.

Classification: architecture boundary (module ownership), no behavior
change.

## Scope

Create `crates/orca-tui/src/session_picker.rs` (registered in lib.rs
between `selection` and `session_picker_actions`) owning an
`impl AppState` block with the eleven methods above, moved verbatim.
All ten public methods keep their exact signatures. The private helper
becomes `pub(crate)`: Rust method privacy is scoped to the impl block's
module, and `reset_session_selection_to_first_match` has a third caller
that stays in `types.rs` — the `update` reducer's session-backfill
handler (types.rs:2553). No logic edits, no other visibility changes.
The new module needs only `use crate::types::AppState;`.

No baseline drift expected: the runtime-surface-contract inventories
record no session-picker sites (verified by grep); the validators still
run as the acceptance gate.

## Non-Goals

- No move of `session_picker_actions.rs` (key dispatch stays), the
  `update` reducer, session field definitions, or
  `SessionPickerAction`s.
- No CLI/TUI-flow/server/JSONL/persistence changes.
- No source-line-count assertions; the existing tests are the oracle.

## Ownership

`session_picker.rs` owns picker querying/selection state transitions;
`session_picker_actions.rs` owns key dispatch into them; `types.rs` owns
the fields and the `update` reducer.

## Normal / Failure Semantics

Unchanged: empty query matches every session; navigation clamps/cycles
within the filtered set; query edits reset selection to the first match.

## Acceptance

1. The eleven methods live in `session_picker.rs` (impl AppState);
   types.rs no longer defines them; compile clean.
2. Behavioral oracle unchanged and green:
   - `cargo test -p orca-tui --lib --locked` (1034 tests)
   - `cargo test --test tui_pty_contract --locked -- --test-threads=1` (6 tests)
3. `cargo fmt --all -- --check` and `git diff --check` clean.
4. Both surface-contract validators green (baseline-maintenance step):
   - `node --test scripts/test-validate-runtime-surface-contract.mjs`
   - `node --test scripts/test-validate-windows-platform-boundaries.mjs`
5. Diff review: relocation only.

## Rollback

Single revertible commit; no persisted state.

## Migration

No temporary state; the old paths (methods in types.rs) are removed in the
same commit, not kept as wrappers.
