# TUI Renderer Input Wake Ownership Plan

**Goal:** Give renderer-side input receiver ownership and the terminal
suspend/resume handshake one focused owner without changing priority,
backpressure, routing, frame, terminal, or compatibility behavior.

**Architecture:** Add private `RendererInputWakeOwner` in
`renderer_input_wake.rs`. It consumes `TerminalInputReceivers`, delegates one
biased selection to `input_wake.rs`, filters admitted events, and owns the
existing acknowledgement/blocking/resume/error protocol. The app retains
semantic input routing and supplies the existing frame-resume callback.

**Tech stack:** Rust, crossbeam-channel, tokio oneshot, crossterm, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Tests

- [x] Audit input receiver creation/transfer, control/focus/ordinary priority,
  event cap and filtering, suspend acknowledgement, repeated suspend, resume,
  exact disconnect/error behavior, renderer-frame resume, downstream tests,
  validators, roadmap evidence, history, and source counts.
- [x] Create an isolated worktree from clean local `main` and write this
  Proposed spec and plan before production edits.
- [x] Register a private `renderer_input_wake` module containing only direct
  tests for the absent owner API.
- [x] Run `cargo test -p orca-tui renderer_input_wake --lib --locked --
  --test-threads=1`; require RED because the production owner is absent.

## Task 2: Implement The Wake Owner

- [x] Add `RendererInputWakeOwner` that consumes the existing three receiver
  clones and stores the unchanged ordinary input limit.
- [x] Delegate biased selection to `receive_prioritized_input_or_control`,
  filter the same pointer-motion events, and preserve timeout/resumed behavior.
- [x] Move first acknowledgement, suspended control wait, repeated suspend,
  resumed callback, and exact error translation out of `run_tui_inner`.
- [x] Replace direct receiver polling in `app.rs` with one owner call while
  leaving semantic input routing and frame resume ownership unchanged.
- [x] Run owner tests GREEN, then focused lower-level input-wake,
  input-runtime signal/Drop, presentation resume, renderer-frame, and
  suspend/focus regression tests.

## Task 3: Freeze Contracts And Evidence

- [x] Add a closed `renderer_input_wake` TUI entrypoint with exact app and
  owner anchors; update the Rust mirror without broadening baselines.
- [x] Add deletion-style negative self-tests for app delegation, receiver
  transfer, priority call, filtering, first/repeated acknowledgement, resumed
  callback, and both disconnect paths while masking references remain.
- [x] Regenerate the manifest digest and update roadmap ownership, accurate
  source counts, next-boundary wording, implemented spec evidence, and this
  plan only after behavior passes.
- [x] Run `cargo check -p orca-tui --tests --locked`, ordinary
  `cargo check -p orca-tui --locked`, both validators and self-tests,
  `cargo fmt --all -- --check`, and `git diff --check`.

## Task 4: Review, Integrate, And Clean Up

- [x] Run `cargo test -p orca-tui --lib --locked -- --test-threads=1` and root
  `cargo test --test tui_pty_contract --locked -- --test-threads=1`.
- [x] Run CodeRabbit independent review focused on receiver ownership,
  priority/backpressure, filtering, acknowledgement order, repeated suspend,
  resume callback/error propagation, disconnect text, dependency direction,
  validator integrity, and compatibility; resolve every Critical or Important
  finding.
- [x] Commit once as `refactor(tui): own renderer input wake`.
- [x] Rebase onto latest local `main` and repeat affected and full gates.
- [x] Fast-forward local `main`, repeat root full gates, then immediately remove
  only this worktree and merged topic branch.
