# TUI Plan Panel State Ownership

## Status

Implemented on `codex/tui-plan-panel-owner`, based on local `main` at
`a8dfd822a`; pending fast-forward to that local branch.

## Problem And Evidence

`AppState` directly owns two public mutable plan-panel facts in
`crates/orca-tui/src/types.rs`: `current_plan` and `plan_update_failed`.
Their transitions are spread across the reducer, session reset, turn
completion, history restoration, and renderer. The state is not a second
runtime truth: it is one TUI presentation model fed by existing
`PlanUpdated` and `HistoryLoaded` events. Its ownership is nevertheless
unclear, and renderer/tests can mutate the facts directly.

The adjacent surface-only Plan convergence is deliberately out of scope.
`SurfacePlanSnapshot` is complete for typed updates, but legacy transcript
recovery can still populate `HistoryLoaded.plan` after a surface has no
structured plan. Removing that source before runtime hydration exists would
lose a restored plan.

## Scope

Create `crates/orca-tui/src/plan_panel.rs`, registered by `lib.rs`, with a
private `PlanPanelState` that owns exactly:

- the optional live `(explanation, items)` plan;
- the stale marker set after a failed `update_plan` tool result;
- replacement from `PlanUpdated` and history restoration;
- clearing on session reset and empty update; and
- transfer of a nonempty plan for turn-end archiving.

`AppState` contains one crate-private `plan_panel: PlanPanelState` field and
offers immutable public queries `current_plan()` and `plan_update_failed()`.
It delegates all plan-panel fact transitions to plan-panel methods. The
existing `archive_current_plan` continues to own transcript message creation;
it requests an owned plan value from the aggregate, then pushes one
`ChatMessage::PlanUpdate`.

`ui.rs` reads only the immutable queries. Tests use event-driven transitions
or narrow cfg(test) setup helpers rather than direct field mutation.

## Non-Goals

- Do not delete or translate `TuiEvent::PlanUpdated`.
- Do not add `SurfacePlanSnapshot` to `SurfaceProjectionState`, change
  `TuiSurfaceProjection`, or alter event ordering.
- Do not modify runtime plan persistence, history JSONL, server/JSONL/ACP
  protocols, plan tool admission, plan parsing, renderer layout, or visible
  strings.
- Do not introduce a plan cache, background worker, compatibility wrapper, or
  additional AppState plan field.

## State And Transition Contract

`PlanPanelState::apply_update(explanation, items)` clears the stale marker and
replaces the live plan. An empty item vector means no live panel. This is the
same behavior as the existing `PlanUpdated` reducer branch.

`restore(plan)` installs an optional historical plan without changing stale
state; `HistoryLoaded` invokes it only when it carries a plan, preserving the
existing no-plan behavior. `reset_for_session()` removes the plan and stale
marker during a session boundary; and `mark_update_failed()` changes only the
stale marker. `take_for_archive()`
clears the stale marker, removes the live plan, and returns it only when its
item vector is nonempty. Thus a completed turn archives exactly one current
plan and leaves the panel empty, while an unsuccessful update leaves the last
plan visible and marked stale until a successful update or session boundary.

The `current_plan()` and `plan_update_failed()` getters preserve public read
access. Replacing the public fields is an intentional source-level API change:
callers must move reads to the getters and drive writes through
`AppState::update` or the existing user-action flow instead of mutating
presentation facts.

## Acceptance

1. `PlanPanelState` is the only owner of the live plan and stale marker;
   `AppState` has one `plan_panel` field and no `current_plan` or
   `plan_update_failed` field.
2. A focused RED/GREEN test proves empty update, failed update, successful
   replacement, reset, and archive transfer preserve the described contract.
3. Existing reducer behavior remains unchanged: restored plans appear,
   failed plan updates retain the last plan and stale badge, session completion
   archives one plan message, and reset removes both facts.
4. The renderer accesses only immutable AppState queries; no production code
   outside `plan_panel.rs` mutates plan-panel facts directly.
5. `cargo test -p orca-tui plan_ --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui --lib --locked -- --test-threads=1`, and
   `cargo test --test tui_pty_contract --locked -- --test-threads=1` pass.
6. `node --test scripts/test-validate-runtime-surface-contract.mjs`,
   `node --test scripts/test-validate-windows-platform-boundaries.mjs`,
   `node scripts/validate-runtime-surface-contract.mjs`,
   `cargo fmt --all -- --check`, and `git diff --check` pass.

## Migration And Rollback

This is an internal TUI-state migration with no persistent-data or wire-format
change. One semantic commit deletes the old fields in the same change that
adds the aggregate and getters. Reverting that commit restores the former
layout. This slice does not authorize a push, tag, GitHub release, or npm
publication.

## Spec Self-Review

The spec keeps current typed and legacy history inputs intact, explicitly
defines update, failure, reset, archive, renderer, and API behavior, and
defers the unresolved runtime hydration problem rather than hiding it in a
local snapshot cache.
