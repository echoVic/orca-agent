# TUI Renderer Frame Ownership Plan

**Goal:** Give renderer frame timing, animation coordination, resume redraw,
bounded iteration scheduling, and presentation completion one focused owner
without changing TUI behavior or compatibility.

**Architecture:** Add a private `RendererFrameOwner` that contains the existing
`FrameScheduler`. `app.rs` keeps terminal/input/runtime construction and routes
input/runtime events; the owner prepares each iteration, runs the existing
fair scheduler, and completes clipboard/title/draw output through the current
renderer and terminal-presentation APIs.

**Tech stack:** Rust, ratatui test backend, crossterm/qwertty presentation,
crossbeam channels, Cargo tests, Node contract validators.

## Task 1: Spec Gate And RED Owner Tests

- [x] Audit the frame loop, scheduler, edit-highlight result flow, animation
  order, copy-notice expiry, suspend/resume path, clipboard delivery, title and
  notification writes, draw acknowledgement, validator baselines, downstream
  tests, roadmap evidence, and source counts.
- [x] Write the Proposed spec before production edits and create the isolated
  worktree from clean local `main`.
- [x] Add the private `renderer_frame` module and direct tests through the
  absent `RendererFrameOwner` API for expiring-copy redraw and presentation
  completion.
- [x] Run `cargo test -p orca-tui renderer_frame --lib --locked --
  --test-threads=1`; require RED because the owner API does not exist.

## Task 2: Implement The Owner

- [x] Add `RendererFrameOwner` with the existing scheduler as its only owned
  state and preserve the initial-draw watermark.
- [x] Move edit-highlight admission, animation demand/expiry/ticks/drag, and
  poll-timeout calculation into `prepare_iteration` without changing order.
- [x] Move resume redraw invalidation and the existing bounded
  `run_event_loop_iteration` call behind owner methods.
- [x] Move staged clipboard consumption, best-effort pending title/notification
  output, scheduled terminal draw, and post-success `did_draw` into
  presentation completion.
- [x] Keep input wake/suspend control, interaction acknowledgement draining,
  key/mouse routing, runtime-event reduction, composer sync, terminal cleanup,
  and agent shutdown in their current owners.
- [x] Move the existing edit-highlight frame tests to the new owner and run the
  owner suite GREEN, then frame-scheduler, presentation, terminal-presentation,
  input-wake, edit-highlight, mouse-selection, and suspend/resume tests.

## Task 3: Freeze Contracts And Evidence

- [x] Add a closed `renderer_frame` TUI entrypoint with path-specific app and
  owner anchors; attribute staged clipboard consumption to the new owner.
- [x] Add negative validator self-tests that independently delete the app owner
  call, iteration preparation, clipboard take, pending presentation output,
  draw, and successful-draw acknowledgement while masking references remain.
- [x] Regenerate the reviewed manifest digest and update the roadmap owner
  inventory, boundary count, accurate source counts, implemented spec evidence,
  and this plan only after behavior passes.
- [x] Run `cargo check -p orca-tui --tests --locked`, ordinary
  `cargo check -p orca-tui --locked`, runtime and Windows validators plus
  self-tests, `cargo fmt --all -- --check`, and `git diff --check`.

## Task 4: Review, Integrate, And Clean Up

- [x] Run `cargo test -p orca-tui --lib --locked -- --test-threads=1` and root
  `cargo test --test tui_pty_contract --locked -- --test-threads=1`.
- [x] Run CodeRabbit independent review focused on frame ordering, final notice
  redraw, edit-highlight dirty admission, resume invalidation, clipboard
  exactly-once delivery, title-before-draw output, draw-error scheduling,
  dependency direction, validator integrity, and compatibility; resolve every
  Critical or Important finding.
- [x] Commit once as `refactor(tui): own renderer frames`.
- [x] Rebase onto latest local `main` and repeat affected and full gates.
- [x] Fast-forward local `main`, repeat root full gates, then immediately remove
  only this worktree and merged topic branch.
