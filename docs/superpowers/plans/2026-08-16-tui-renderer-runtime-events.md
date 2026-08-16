# TUI Renderer Runtime Event Ownership Plan

**Goal:** Give renderer-side runtime-event admission, special-event
coordination, deferred initial-prompt consumption, and mention-search lifecycle
one focused owner without changing runtime, UI, or compatibility behavior.

**Architecture:** Add a private `RendererRuntimeEventOwner` that contains the
existing `MentionSearchManager` and deferred initial prompt. `app.rs` keeps the
terminal/frame loop and lends its mutable renderer state to the owner for one
event or one composer-sync step; `runtime_event_actions` remains the admitted
event reducer.

**Tech stack:** Rust, crossbeam channels, ratatui/text-area renderer state,
runtime typed surfaces, Cargo tests, Node contract validators.

## Task 1: Spec Gate And RED Owner Tests

- [x] Audit the production event branch, attachment admission, prompt
  eligibility/consumption, mention sync/shutdown, config mirroring, validator
  baseline, downstream tests, roadmap evidence, and source counts.
- [x] Write the Proposed spec before production edits and create the isolated
  worktree from clean local `main`.
- [x] Add the `renderer_runtime` module and direct tests through the absent
  `RendererRuntimeEventOwner` API.
- [x] Run `cargo test -p orca-tui renderer_runtime --lib --locked --
  --test-threads=1`; require RED because the owner API does not exist.

## Task 2: Implement The Owner

- [x] Add `RendererRuntimeEventOwner` with the existing mention manager and
  deferred prompt as its only owned state.
- [x] Move attachment admission and all six special runtime-event branches from
  `run_tui_inner` into the owner without changing branch order, payloads, or
  reducer calls.
- [x] Move per-iteration composer/mention synchronization and explicit mention
  shutdown behind the owner.
- [x] Keep terminal/input/frame ownership in `app.rs`; remove only imports and
  mutable locals made obsolete by the extraction.
- [x] Run the direct owner tests GREEN, then affected attachment, runtime-event,
  mention, startup/history, settings, interaction-ack, and frame-loop tests.

## Task 3: Freeze Contracts And Evidence

- [x] Move the `mention_search.shutdown` mutation baseline from `app.rs` to
  `renderer_runtime.rs` and add path-specific production anchors for admission,
  prompt consumption, and reducer delegation.
- [x] Add or adjust negative validator self-tests so imports, enum variants,
  tests, or an owner shell cannot mask deletion of the production paths.
- [x] Regenerate the reviewed manifest digest and update the roadmap owner
  inventory, boundary count, source counts, implemented spec evidence, and this
  plan only after behavior passes.
- [x] Run compiler check, runtime and Windows validators plus self-tests,
  formatter, and diff checks.

## Task 4: Review, Integrate, And Clean Up

- [x] Run the full serial TUI library suite and root-package PTY contract.
- [x] Request independent review focused on admission ordering, prompt
  exactly-once semantics, config mirroring, mention worker lifecycle,
  dependency direction, validator integrity, and compatibility; resolve every
  Critical or Important finding.
- [x] Commit once as `fix(tui): own renderer runtime events`.
- [x] Rebase onto latest local `main` and repeat affected and full gates.
- [x] Fast-forward local `main`, repeat root full gates, then immediately remove
  only this worktree and merged topic branch.
