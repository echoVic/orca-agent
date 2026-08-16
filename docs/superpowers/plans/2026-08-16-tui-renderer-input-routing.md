# TUI Renderer Input Routing Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the renderer's semantic input ordering one focused private
router without changing any low-level action, shortcut, frame, terminal,
protocol, persistence, or visible TUI behavior.

**Architecture:** Add `RendererInputRouter`, a one-dispatch borrowed context
whose `route` method preserves the current scroll/focus/insert-escape/paste/
resize/mouse/key order and returns the existing `io::Result<Option<i32>>`.
`app.rs` constructs it only in the input iteration arm; all existing policy
helpers and the runtime-event arm remain where they are.

**Tech Stack:** Rust, crossbeam-channel, crossterm, tui-textarea, Cargo tests,
Node contract validators.

---

### Task 1: Spec Gate And RED Router Tests

**Files:**

- Create: `docs/superpowers/specs/2026-08-16-tui-renderer-input-routing.md`
- Create: `docs/superpowers/plans/2026-08-16-tui-renderer-input-routing.md`
- Create: `crates/orca-tui/src/renderer_input_router.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [x] Audit the production block, every delegated helper, direct and downstream
  tests, validator inventory, roadmap evidence, and exact base source counts.
- [x] Create the isolated `codex/tui-renderer-input-routing` worktree from clean
  local `main` and write the Proposed spec and this plan before production code.
- [x] Register `mod renderer_input_router;` and add tests that import the absent
  `RendererInputRouter` and exercise real `AppState`, `TextArea`, `VimState`,
  `TerminalPresentation`, hit testing, action channels, and clear callbacks.
- [x] Cover focus short circuit, scroll/paste insert-escape flush, resize
  short circuit, mouse-confirmed plan action, real-key preflight, and clear
  error propagation as separate tests.
- [x] Run:

  ```text
  cargo test -p orca-tui renderer_input_router --lib --locked -- --test-threads=1
  ```

  Require RED with unresolved `RendererInputRouter`; fix only test mistakes
  until the failure proves the production owner is absent.

### Task 2: Implement The Borrowed Routing Owner

**Files:**

- Modify: `crates/orca-tui/src/renderer_input_router.rs`
- Modify: `crates/orca-tui/src/app.rs:326-449`

- [x] Add a private two-lifetime router that borrows the existing collaborators:

  ```rust
  pub(crate) struct RendererInputRouter<'a, 'text> {
      state: &'a mut AppState,
      config: &'a mut RunConfig,
      shared_config: &'a Arc<Mutex<RunConfig>>,
      action_tx: &'a mpsc::Sender<UserAction>,
      preloaded_transcript: &'a Arc<Mutex<Option<SessionTranscript>>>,
      textarea: &'a mut TextArea<'text>,
      vim_state: &'a mut VimState,
      theme: &'a Theme,
      presentation: &'a mut TerminalPresentation,
      initial_prompt: &'a Option<String>,
  }
  ```

- [x] Implement `route` with the current return contract and lazy clear
  callback:

  ```rust
  pub(crate) fn route(
      mut self,
      input: BatchedInputEvent,
      now: Instant,
      clear_terminal: impl FnMut() -> io::Result<()>,
  ) -> io::Result<Option<i32>>
  ```

- [x] Preserve scroll flush/cancel/action order, focus early return,
  insert-escape resolution, paste/mouse preflush, paste/resize/mouse short
  circuits, and inert non-key behavior exactly.
- [x] Preserve direct synthetic-Enter status dispatch, real-key preflight then
  status dispatch, exact collaborator identity, cloned initial prompt, exit
  code folding, and unmodified `io::Error` propagation.
- [x] Replace only the `IterationEvent::Input` match body in `app.rs` with one
  `RendererInputRouter::new(...).route(...)` call. Keep input coalescing,
  runtime routing, frame ownership, and terminal cleanup unchanged.
- [x] Run the exact router test command GREEN, then run:

  ```text
  cargo test -p orca-tui input_event_actions --lib --locked -- --test-threads=1
  cargo test -p orca-tui insert_escape --lib --locked -- --test-threads=1
  cargo test -p orca-tui key_event_actions --lib --locked -- --test-threads=1
  cargo test -p orca-tui status_key_actions --lib --locked -- --test-threads=1
  cargo test -p orca-tui renderer_frame --lib --locked -- --test-threads=1
  ```

### Task 3: Close Contracts And Evidence

**Files:**

- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-16-tui-renderer-input-routing.md`
- Modify: `docs/superpowers/plans/2026-08-16-tui-renderer-input-routing.md`

- [x] Add one `renderer_input_routing` entrypoint with exact app delegation and
  owner sequencing anchors; increment the Rust mirror without broadening any
  mutation or harmless-method baseline.
- [x] Add deletion-style negative self-tests for app delegation, scroll flush,
  focus, insert-escape, paste/mouse preflush, resize, handled mouse, synthetic
  Enter, real-key preflight, status folding, and tests/imports that could mask
  deleted production behavior.
- [x] Regenerate the manifest SHA-256 and update only its digest entry.
- [x] Update roadmap ownership/count/next-boundary wording, measured source
  counts, implemented spec evidence, and plan checkboxes only after behavior
  passes.
- [x] Run ordinary and test compiler gates, both validators and self-tests,
  formatter, digest equality, and diff check.

### Task 4: Full Verification, Review, Integration, And Cleanup

**Files:**

- Review all files listed above; do not stage unrelated paths.

- [x] Run full serial TUI and root PTY gates:

  ```text
  cargo test -p orca-tui --lib --locked -- --test-threads=1
  cargo test --test tui_pty_contract --locked -- --test-threads=1
  ```

- [x] Run CodeRabbit against all staged slice files. Resolve every valid
  Critical or Important issue and record 0 unresolved blockers.
- [x] Stage the exact slice files and create one semantic commit:

  ```text
  refactor(tui): own renderer input routing
  ```

- [x] Rebase onto latest local `main`; repeat router, affected, validator,
  formatter, full serial TUI, and PTY gates.
- [x] Fast-forward local `main`; repeat root owner/validator/full TUI/PTY gates.
- [x] Immediately remove only `.worktrees/tui-renderer-input-routing` and
  `codex/tui-renderer-input-routing`, preserve unrelated worktrees, record
  final evidence in the same semantic commit, and verify clean root state.
