# TUI Convergence Slice 1: Insert-Escape Flush Orchestration Extraction

## Status

Proposed for `codex/tui-convergence`, based on `main` at `54f9700fc`.

## Problem And Evidence

`crates/orca-tui/src/app.rs` is 10,165 lines; the roadmap's "TUI/runtime
protocol convergence" item is still open ("renderer-owned orchestration
and projection duplication remain"). The file mixes free-function
orchestration with the AppState impl and ~7,000 lines of tests. The
insert-escape flush policy (free functions at lines 170-232:
`refresh_after_insert_escape_flush`, `resolve_pending_insert_escape_before_routing`,
`flush_pending_insert_escape_before_non_key`, `flush_expired_insert_escape`)
is a coherent, bounded orchestration category — input-ownership
preprocessing for paste/submit/shortcut routing — with a strong existing
behavioral test suite (lines 1295-1566).

Classification: architecture boundary (module ownership), no behavior
change.

## Scope

Move the four free functions (and any small helpers they exclusively use)
into a new `crates/orca-tui/src/insert_escape.rs` module; `app.rs` imports
them. No logic edits.

## Non-Goals

- No AppState impl extraction, no projection changes, no protocol/CLI/
  persistence changes.
- No source-line-count assertions; the existing tests are the oracle.

## Ownership

`insert_escape.rs` owns the insert-escape flush policy; `app.rs` owns
routing and dispatch. Types stay in `types.rs`.

## Acceptance

1. The four functions live in insert_escape.rs; app.rs imports them;
   compile clean.
2. The existing behavioral tests (insert-escape preflight, paste/submit
   ownership, expired flush refresh) pass unchanged:
   `cargo test -p orca-tui --lib --locked` plus the PTY contract
   `cargo test -p orca-tui --test tui_pty_contract --locked -- --test-threads=1`.
3. `cargo fmt --all -- --check` and `git diff --check` clean.
4. Diff review: relocation only.

## Rollback

Single revertible commit; no persisted state.

## Slice 2: Terminal Presentation Lifecycle Extraction

### Scope

Move the five terminal-presentation free functions
(`resume_terminal_render`, `initialize_terminal_presentation`,
`complete_presentation_resume`, `finish_terminal_presentation`,
`with_terminal_presentation_cleanup`, app.rs:732-790) into
`crates/orca-tui/src/presentation.rs`. They are generic over the terminal
target with no AppState coupling; pure relocation, app.rs imports the
module. Same acceptance as slice 1 (orca-tui lib + tui_pty_contract +
fmt + relocation-only diff).

## Slice 3: Input Wake Selection Extraction

### Scope

Move the input wake-selection category (`InputWake` enum,
`receive_prioritized_input_or_control`, and the two test-only helpers
`receive_input_batch` / `receive_input_or_control`, app.rs:739-851) into
`crates/orca-tui/src/input_wake.rs`. Pure relocation; the test helpers
keep their `#[cfg(test)]` gating inside the new module and stay visible
to the app.rs tests via the parent import. Same acceptance as slices 1-2
(orcaui lib + tui_pty_contract + fmt + relocation-only diff).

## Slice 4: Workspace Root And Syntax State Configuration

### Scope

Move the workspace-config category (`mention_search_roots`,
`syntax_workspace_root`, `configure_tui_syntax_state`,
`configure_and_preload_tui_state`, app.rs:753-800) into
`crates/orca-tui/src/workspace_config.rs`. The edit-highlight polling
pair stays (thin AppState delegates, not orchestration). Pure relocation;
same acceptance as slices 1-3.

## Slice 5: Terminal Scrollback Clear Extraction

### Scope

Move `clear_terminal_scrollback_with` and `clear_terminal_scrollback`
(app.rs:757-800) into `crates/orca-tui/src/scrollback.rs` (the
`InlineTerminal` alias import moves along from presentation.rs usage —
import only, the alias stays in presentation.rs). Pure relocation; same
acceptance as slices 1-4.

## Slice 6: Exit Policy Extraction

### Scope

Move `exit_resume_hint` and `exit_session_id` (app.rs:164-181) into
`crates/orca-tui/src/exit_policy.rs` — the resume-hint formatting and
saved-session id resolution. Pure relocation; same acceptance as slices
1-5.

## Slice 7: Hosted Side Parent Extraction

### Scope

Move the hosted-side parent category (`HostedSideParent` struct and the
four helpers `shutdown_attached_side_on_controller_exit`,
`side_parent_status_for_runtime_thread`, `hosted_config_for_active`,
`rotate_attached_event_sender`, app.rs:91-163) into
`crates/orca-tui/src/hosted_side.rs`, and the `TuiExit` struct into
`exit_policy.rs` (the exit type belongs with the exit policy). Pure
relocation; same acceptance as slices 1-6.

## Baseline Maintenance (added after the CI baseline fix)

Any slice that relocates a function listed in the reviewed TUI
inventories of `scripts/validate-runtime-surface-contract.mjs` must
refresh those inventories in the same commit:

- `BASELINE_DIRECT_TUI_MUTATION_SITES`
- `BASELINE_HARMLESS_SAME_NAME_METHOD_SITES`
- `BASELINE_HARMLESS_ASSOCIATED_FUNCTION_ITEM_SITES`
- `BASELINE_HARMLESS_ASSOCIATED_FUNCTION_SHA256`
- `BASELINE_UNRESOLVED_USER_ACTION_SEND_SITES` and its SHA256 map

Oracle: `node --test scripts/test-validate-runtime-surface-contract.mjs`.
The exported `scanTuiMutation*` helpers dump the current inventory for
regeneration. Slices 2 and 5 missed this step and red-lined CI on main
until the dedicated fix (see
`2026-08-13-ci-surface-contract-baseline-drift.md`).

## Slice 8: Workflow Panel And Background Approval Routing Extraction

### Scope

Move the workflow-panel AppState methods (`show_workflows`, `show_agents`,
`select_previous_workflow_task`, `select_next_workflow_task`,
`open_selected_background_approval_dialog`, `push_pending_workflow_notification`,
`show_conversation`, `apply_workflow_tasks_update`) and their exclusive
helpers (`push_pending_workflow_notification_unique`,
`sort_workflow_tasks_for_panel`, the three `is_*_main_session` predicates,
`workflow_task_panel_group`, `workflow_task_activity_ms`) out of
`types.rs` into `crates/orca-tui/src/workflow_panel.rs`. Relocation only:
the two private methods become `pub(crate)` (Rust method privacy is scoped
to the impl block's module); `types.rs` imports back the two helpers it
still uses. Same acceptance as slices 1-7 plus both surface-contract
validators (baseline-maintenance step). Full spec:
`2026-08-13-tui-convergence-workflow-panel.md`.

## Slice 9: Transcript Search Orchestration Extraction

### Scope

Move the six transcript-search AppState methods (`open_transcript_search`,
`close_transcript_search`, `replace_transcript_search_query`,
`refresh_transcript_search`, `search_next`, `search_previous`,
types.rs:2174-2230) into `crates/orca-tui/src/transcript_search.rs` as an
additional `impl AppState` block next to `TranscriptSearchState`. All six
are already `pub(crate)`; no visibility change. No baseline drift
(verified by the validators). Same acceptance as slices 1-8. Full spec:
`2026-08-13-tui-convergence-transcript-search.md`.
