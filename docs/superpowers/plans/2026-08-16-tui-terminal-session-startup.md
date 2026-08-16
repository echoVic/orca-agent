# TUI Terminal Session Startup Ownership Plan

**Goal:** Give TUI terminal-session assembly and the pending-to-active startup
transition one focused owner without changing input, presentation, renderer,
runtime, or compatibility behavior.

**Architecture:** Add private `PendingTerminalSession` and
`ActivatedTerminalSession` states in `terminal_session.rs`. The pending state
owns the existing input runtime, resolved theme, receiver clones, terminal
presentation, and backend. `app.rs` retains hosted-agent startup, then consumes
the pending state to construct and clear ratatui before entering the existing
presentation cleanup scope.

**Tech stack:** Rust, qwertty, crossterm, ratatui test backend, crossbeam
channels, Cargo tests, Node contract validators.

## Task 1: Spec Gate And RED Tests

- [x] Audit terminal/input startup order, qwertty ownership and failure paths,
  theme/profile derivation, receiver cloning, presentation construction,
  backend/ratatui construction, startup clear, initial title/draw, cleanup
  order, agent-startup failure precedence, validators, downstream tests,
  roadmap evidence, history, and source counts.
- [x] Create an isolated worktree from clean local `main` and write this
  Proposed spec and plan before production edits.
- [x] Register a private `terminal_session` module containing only direct tests
  for the absent pending/activated API, startup clear/resource preservation,
  and original-agent versus input-finish error precedence.
- [x] Run `cargo test -p orca-tui terminal_session --lib --locked --
  --test-threads=1`; require RED because the production owner API is absent.

## Task 2: Implement The Two-State Owner

- [x] Add `PendingTerminalSession` that starts the existing `InputRuntime`,
  resolves the theme/profile, clones all three receiver lanes, constructs
  `TerminalPresentation`, and owns the existing capability backend.
- [x] Add the generic error-precedence helper and pending-owner failure method
  so agent startup failure finishes input before returning the exact current
  winning error.
- [x] Add `ActivatedTerminalSession`; activation constructs and clears ratatui
  only after hosted-agent startup, then exposes the same theme, receivers, and
  presentation-cleanup resources to `app.rs`.
- [x] Replace direct assembly in `run_tui_inner` with owner calls while keeping
  hosted runtime/state/composer/frame/shutdown orchestration in place.
- [x] Keep `input_runtime.rs`, `presentation.rs`, `renderer_frame.rs`, and
  `renderer_runtime.rs` semantics and public visibility unchanged.
- [x] Run the owner suite GREEN, then focused input-runtime,
  terminal-presentation, presentation lifecycle, renderer-frame, input wake,
  suspend/resume, and startup failure/order tests.

## Task 3: Freeze Contracts And Evidence

- [x] Add a closed `terminal_session_startup` TUI entrypoint with exact app and
  owner anchors; update the Rust mirror inventory without broadening existing
  baselines.
- [x] Add deletion-style negative validator self-tests for pending-owner
  construction, agent failure finish routing, activation, production input
  start, terminal construction, and startup clear while masking references
  remain.
- [x] Regenerate the reviewed manifest digest and update roadmap ownership,
  next-boundary wording, accurate source counts, implemented spec evidence,
  and this plan only after behavior passes.
- [x] Run `cargo check -p orca-tui --tests --locked`, ordinary
  `cargo check -p orca-tui --locked`, both validators and self-tests,
  `cargo fmt --all -- --check`, and `git diff --check`.

## Task 4: Review, Integrate, And Clean Up

- [x] Run `cargo test -p orca-tui --lib --locked -- --test-threads=1` and root
  `cargo test --test tui_pty_contract --locked -- --test-threads=1`.
- [x] Run CodeRabbit independent review focused on pending/active state
  ownership, input-before-terminal order, agent/finish error precedence,
  activation clear, title-before-draw, terminal-drop-before-input-finish,
  unwind behavior, dependency direction, validator integrity, and
  compatibility; resolve every Critical or Important finding.
- [x] Commit once as `refactor(tui): own terminal session startup`.
- [x] Rebase onto latest local `main` and repeat affected and full gates.
- [x] Fast-forward local `main`, repeat root full gates, then immediately remove
  only this worktree and merged topic branch.
