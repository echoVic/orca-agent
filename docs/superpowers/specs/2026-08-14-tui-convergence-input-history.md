# TUI Convergence Slice 11: Input History Ownership Extraction

## Status

Implemented and verified on `codex/tui-convergence-input-history`, based on
`main` at `62f22aa58`.

## Problem And Evidence

`crates/orca-tui/src/types.rs` combines aggregate state with the complete input
history policy: local file discovery, project grouping, bounded loading,
append-only writes, duplicate suppression, and draft-restoring navigation.
That policy is currently split between constructor initialization, four free
helpers at `types.rs:2215-2297`, and four AppState methods at
`types.rs:2394-2441`.

The real input path dispatches navigation from `idle_key_actions.rs` through
`composer_input_actions.rs` to `AppState::history_previous` and
`AppState::history_next`. Composer mutations, queued-message restores, and
session reset call `reset_history_navigation`; direct, slash-command, and
queued submissions call `record_prompt`. These are one coherent policy
boundary; key routing and aggregate state have separate owners.

Classification: architecture boundary, no intended user-visible behavior
change.

## User Value And Scope

History recall must preserve the unsent draft while browsing older prompts and
restore it exactly after Down moves past the newest saved prompt. Create
`crates/orca-tui/src/input_history.rs`, registered in `lib.rs`, and move without
semantic changes:

- `input_history_path`, `current_project`, `load_input_history`, and
  `append_input_history`;
- `AppState::{record_prompt, history_previous, history_next,
  reset_history_navigation}`.

`types.rs` retains the aggregate fields and constructs `input_history` using
`input_history::load_input_history()`. All four AppState method signatures stay
unchanged, so TUI routing keeps its existing contract.

## Non-Goals And Compatibility

No key-routing, composer, Vim, JSONL history format, history location, project
ordering, maximum length, CLI, TUI workflow, server/JSONL protocol, runtime
ownership, cancellation, or persisted session behavior changes. Do not add a
compatibility wrapper, second history cache, or source-shape test.

The AppState API, `.orca/history.jsonl` format and location, and session
persistence remain compatible.

## Ownership And Semantics

`input_history.rs` owns history I/O and its transition policy. `AppState`
remains the only aggregate fact source for `input_history`, `history_cursor`,
and `draft_before_history`; it holds no second cache or state machine.
`composer_input_actions.rs` and `queued_input_actions.rs` own routing and call
the unchanged AppState API.

Missing or malformed files retain current best-effort behavior. Loading selects
at most 500 unique prompts from at most 1,000 valid recent records, current
project first. Recording appends only when distinct from the newest in-memory
prompt and clears navigation. Up saves a draft and navigates backward to the
oldest entry; Down moves forward then restores that draft and clears navigation.
Empty history or inactive Down leaves the composer unchanged. Session reset and
ordinary mutations clear navigation through the existing public method.

## Acceptance

1. `input_history.rs` holds all four I/O helpers and four AppState methods;
   `types.rs` retains none of their definitions and initializes the same vector
   through the relocated loader.
2. A characterization test proves backward clamp, forward navigation, draft
   restoration, and reset. Existing submission tests continue proving prompt
   recording.
3. `cargo test -p orca-tui --lib --locked -- --test-threads=1` and
   `cargo test --test tui_pty_contract --locked -- --test-threads=1` pass.
4. `node --test scripts/test-validate-runtime-surface-contract.mjs` and
   `node --test scripts/test-validate-windows-platform-boundaries.mjs` pass;
   the reviewed runtime-surface manifest anchors and its digest are updated
   together with the relocated input-history inventory paths.
5. `cargo fmt --all -- --check`, `git diff --check`, and review show one
   policy owner with no protocol, persistence, or duplicate implementation.

## Migration And Rollback

One semantic commit creates the module, moves every named policy definition,
updates the required inventory, and deletes old definitions in the same change.
There is no persisted migration or dual path. One revert restores the previous
source layout without data recovery.
