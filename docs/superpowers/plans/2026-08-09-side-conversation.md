# Side Conversation implementation plan

> Execute in `/Users/qingyun/Documents/GitHub/blade-deepseek/.worktrees/side-conversation`.
> Keep `main` and all other worktrees untouched until integration.

## Task 1: Runtime child model

Files: `crates/orca-runtime/src/runtime_host.rs`,
`crates/orca-runtime/src/thread.rs`, `crates/orca-runtime/src/session.rs`,
`crates/orca-runtime/src/runtime_surface/{projection,commands}.rs`.

- Add an explicit `RuntimeThreadStartRequest` path for an attached ephemeral
  child: disabled history, fresh process-local identity, parent surface ID,
  inherited `Conversation`, and no one-shot auto-close.
- Add the side boundary instruction in runtime/session code, not in TUI.
- Preserve MCP/settings needed by the parent while preventing persistence and
  auto-memory writes.
- Expose `RuntimeHostHandle::start_side_thread(parent, config, title)` (or the
  smallest equivalent typed API) so the parent snapshot and child creation are
  runtime-owned and the TUI never constructs a second agent loop.
- Define close semantics on the runtime handle that cancel, settle, join, and
  fence the child actor; parent shutdown remains independent.

## Task 2: RED runtime tests

Files: runtime host/session test modules.

- Add tests for side identity/parent relationship and no session-store entry.
- Add a two-turn side test proving inherited context is visible only as
  reference and a post-boundary prompt is the active request.
- Add parent/side independence and cancellation/close barrier tests, including
  a late-result assertion.
- Run the focused runtime filter and record the expected compile/test failure
  before implementing Task 1.

## Task 3: TUI action and projection model

Files: `crates/orca-tui/src/types.rs`, `shortcuts.rs`,
`slash_command_actions.rs`, `commands/`, `app.rs`, and the relevant render/status
modules.

- Add typed actions/events for start/toggle/close Side and a runtime attachment
  identity for parent versus child.
- Add `/side [question]`, default `Ctrl+/`, footer context, and parent status
  projection. Reuse existing attachment fencing and event reducer machinery.
- Route ordinary operations to the active handle, while parent events update
  only the hidden parent-status state when Side is visible.
- Reject ambiguous/destructive commands in Side with a clear notice.

## Task 4: RED TUI tests and implementation

- Add failing parser/shortcut/reducer tests first, then implement the controller
  state that owns `parent: RuntimeThreadHandle` and optional `side` child.
- Add end-to-end hosted TUI tests for prompt/no-prompt start, toggle, separate
  transcripts, parent approval/status, Side cancel, and rejection of navigation
  while a hidden Side remains attached.
- Ensure every side event sender is rotated/fenced and every child handle is
  joined on close, switch, and controller shutdown.

## Task 5: Verification and integration

- Run focused runtime/TUI tests, full package tests, workspace all-targets/all-
  features tests, formatter, and diff checks.
- Perform the real terminal smoke sequence in the spec and capture observed
  behavior/limitations.
- Request an independent code review, fix all Critical/Important findings,
  rebase onto current `main`, rerun affected gates, then integrate only the
  reviewed commits. Update roadmap/spec evidence and release notes if the
  completed slice is releasable.

## Plan self-review

- Runtime owns snapshot, child identity, persistence, cancellation, and join;
  TUI owns only user intent and projection.
- No durable Side history, implicit merge, detached cleanup, or parallel agent
  loop is introduced.
- Normal, cancel, reject, timeout/failure, disconnect, shutdown, and restart
  behaviors have explicit owners and test gates.
- The plan preserves `/fork` compatibility and includes removal/compatibility
  checks rather than creating a competing session model.
