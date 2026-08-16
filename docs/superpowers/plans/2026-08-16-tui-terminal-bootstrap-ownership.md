# TUI Terminal Bootstrap Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the active terminal owner enforce initial title, first draw, renderer execution, and total cleanup as one unavoidable lifecycle.

**Architecture:** Extend `ActivatedTerminalSession::run` and its generic test seam; keep concrete UI rendering in `app.rs`, generic presentation ordering in `presentation.rs`, foreground iteration in `renderer_loop.rs`, and post-terminal shutdown in `tui_run_lifecycle.rs`. Do not read or migrate registry-only task state.

**Tech Stack:** Rust, ratatui, crossterm/qwertty, Cargo tests, Node validator/self-tests, Git worktrees.

---

### Task 1: Freeze The Active Owner Bootstrap Contract

**Files:**
- Modify: `crates/orca-tui/src/terminal_session.rs`

- [x] **Step 1: Add the failing initialization-order owner test**

Extend `activated_session_owns_input_wake_body_and_total_cleanup` to pass a
separate initializer before the renderer body and assert:

```rust
assert_eq!(
    *calls.borrow(),
    ["initialize", "body", "reset", "drop", "finish"]
);
```

The initializer mutates the generic test terminal so the body can assert that
it ran first. Add `initialization_failure_skips_body_and_still_cleans` with an
initializer returning `io::Error::other("initialize failed")`, a body that
panics if called, and exact `initialize, reset, drop, finish` assertions.

- [x] **Step 2: Run RED and record the expected API failure**

Run:

```bash
cargo test -p orca-tui terminal_session --lib --locked -- --test-threads=1
```

Expected: compilation fails because the current `run_with` method accepts the
renderer body directly and has no distinct initialization callback.

- [x] **Step 3: Add initialization to the generic owner sequence**

Change the private method shape to:

```rust
fn run_with<R, Context>(
    self,
    max_input_events: usize,
    mut context: Context,
    initialize: impl FnOnce(
        &mut Terminal,
        &mut TerminalPresentation,
        &Theme,
        &mut Context,
    ) -> io::Result<()>,
    body: impl FnOnce(
        &mut Terminal,
        &mut TerminalPresentation,
        &RendererInputWakeOwner,
        &Theme,
        &mut Context,
    ) -> io::Result<R>,
    reset_title: impl FnOnce(&mut Terminal, &mut TerminalPresentation) -> io::Result<()>,
    drop_terminal: impl FnOnce(Terminal),
    finish_input: impl FnOnce(&mut Input) -> io::Result<()>,
) -> io::Result<R>
```

Inside `with_terminal_presentation_cleanup`, invoke `initialize` before `body`
with `?`, passing `&mut context` to each call sequentially. Keep both calls
inside the same body-versus-cleanup precedence scope. In both owner tests, use
one `Vec<&str>` context: initializer pushes `initialize`, body observes it and
pushes `body`; the failure case proves the body cannot mutate it.

- [x] **Step 4: Run GREEN for the owner contract**

Run the Task 1 command again. Expected: all terminal-session tests pass and the
new cases prove exact ordering, body suppression, total cleanup, and error
precedence.

### Task 2: Move Production First Presentation Behind The Owner

**Files:**
- Modify: `crates/orca-tui/src/terminal_session.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [x] **Step 1: Extend the production owner API**

Import `initialize_terminal_presentation` and `AppStatus` in
`terminal_session.rs`. Change production `run` to accept:

```rust
pub(crate) fn run<R, Context>(
    self,
    max_input_events: usize,
    initial_status: AppStatus,
    context: Context,
    draw_initial: impl FnOnce(
        &mut InlineTerminal,
        &Theme,
        &mut Context,
    ) -> io::Result<()>,
    body: impl FnOnce(
        &mut InlineTerminal,
        &mut TerminalPresentation,
        &RendererInputWakeOwner,
        &Theme,
        &mut Context,
    ) -> io::Result<R>,
) -> io::Result<R>
```

Bind the generic initializer to:

```rust
initialize_terminal_presentation(
    terminal,
    |terminal| {
        let _ = presentation.write_pending(
            terminal.backend_mut().inner_mut(),
            initial_status,
        );
        Ok(())
    },
    |terminal| draw_initial(terminal, theme, context),
)
```

Leave reset-title, terminal drop, and input finish bindings unchanged.

- [x] **Step 2: Convert `app.rs` to the owner API and delete the old path**

Remove the `initialize_terminal_presentation` import and direct call. Pass
`state.status` and the existing exact first-frame closure before the renderer
body:

```rust
terminal_session.run(
    MAX_INPUT_EVENTS_PER_BATCH,
    state.status,
    (&mut state, &mut textarea),
    |terminal, theme, context| {
        let (state, textarea) = context;
        terminal
            .draw(|frame| ui::render(frame, state, textarea, theme))
            .map(|_| ())
    },
    |terminal, presentation, renderer_input_wake, theme, context| {
        let (state, textarea) = context;
        RendererLoopOwner::new(
            Instant::now(),
            FRAME_INTERVAL,
            ANIMATION_INTERVAL,
            MAX_RUNTIME_EVENTS_PER_BATCH,
            renderer_input_wake,
            &renderer_interaction_acks,
            &renderer_runtime_inbox,
            &mut renderer_runtime,
            state,
            &mut config,
            &shared_config,
            &action_tx,
            &pending_workflow_notifications,
            &preloaded_transcript,
            textarea,
            &mut vim_state,
            theme,
            presentation,
            &initial_prompt,
            &workspace_root,
        )
        .run(
            terminal,
            clear_terminal_scrollback,
            clipboard::copy_to_clipboard,
            |terminal, presentation, status| {
                let _ = presentation
                    .write_pending(terminal.backend_mut().inner_mut(), status);
            },
        )
    },
)
```

Do not change renderer collaborators, callbacks, constants, or exit handling.

- [x] **Step 3: Run focused behavior and compiler gates**

Run:

```bash
cargo test -p orca-tui terminal_session --lib --locked -- --test-threads=1
cargo test -p orca-tui presentation --lib --locked -- --test-threads=1
cargo test -p orca-tui renderer_loop --lib --locked -- --test-threads=1
cargo test -p orca-tui input_runtime --lib --locked -- --test-threads=1
cargo check -p orca-tui --tests --locked
```

Expected: every command exits zero. Existing warnings may remain unchanged;
there must be no new compile error or test failure.

### Task 3: Close The Validator And Documentation Contract

**Files:**
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/superpowers/specs/2026-08-16-tui-terminal-bootstrap-ownership.md`
- Modify: `docs/superpowers/plans/2026-08-16-tui-terminal-bootstrap-ownership.md`
- Modify: `docs/production-roadmap.md`

- [x] **Step 1: Extend the closed terminal lifecycle anchors**

Update the existing `terminal_session_lifecycle` entrypoint so it requires the
owner-side initializer and app-side `state.status` plus first-draw delegation.
Replace any app-owned initialization anchor with a negative assertion that
`app.rs` contains no direct `initialize_terminal_presentation` call.

- [x] **Step 2: Add path-specific negative self-tests**

Add source mutations that remove the owner initializer or restore a direct app
initializer. The validator must reject each mutated fixture even though the
presentation helper and its unit tests still exist.

- [x] **Step 3: Update manifest references and digest**

Adjust exact line references/current payloads after the Rust move. Regenerate
the manifest entry in the repository's existing digest JSON with the exact
SHA-256 procedure used by the validator, then verify the digest matches.

- [x] **Step 4: Update Spec, plan, and roadmap evidence**

Set the Spec to Implemented only after behavior gates pass. Record RED/GREEN
evidence, final file counts, validation commands, review status, and the next
honest boundary: a separate runtime-owned cold legacy registry reconciliation
Spec. Increment the focused TUI owner count from forty-three to forty-four.

- [x] **Step 5: Run validator and mechanical gates**

Run:

```bash
node scripts/validate-runtime-surface-contract.mjs
node --test scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
rg -n 'initialize_terminal_presentation' crates/orca-tui/src/app.rs
```

Expected: validators/self-tests, formatter, and diff check exit zero; the final
search exits one with empty output because the obsolete app path is gone.

### Task 4: Full Verification, Review, Commit, Rebase, Integration, And Cleanup

**Files:**
- Review all files changed by Tasks 1-3.

- [x] **Step 1: Run full local gates**

Run:

```bash
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: full serial TUI and all six PTY tests pass with zero failures.

- [x] **Step 2: Run scoped code review and resolve severity gates**

Run CodeRabbit against this worktree's uncommitted diff. Fix every Critical and
Important issue with a new RED regression when behavior changes; rerun affected
focused and full gates. Record any service failure exactly and do not call a
manual inspection a CodeRabbit result.

- [x] **Step 3: Create one semantic commit**

Stage only the Spec, plan, roadmap, Rust owner/app files, validator/self-test,
manifest, and digest. Commit once with:

```bash
git commit -m "refactor(tui): own terminal bootstrap"
```

- [x] **Step 4: Rebase latest local `main` and reverify**

Fetch origin, confirm whether remote `main` advanced, rebase the topic branch
onto current local `main`, and rerun focused owner/presentation tests,
validators, formatter, diff check, full TUI, and PTY gates.

- [x] **Step 5: Fast-forward local `main`, verify root, and clean promptly**

From the root checkout, require a clean `main`, fast-forward it to the topic
commit, rerun focused owner/presentation tests, full TUI, PTY, validators,
formatter, and diff check. Only after successful integration, remove exactly:

```text
.worktrees/tui-terminal-bootstrap-owner
codex/tui-terminal-bootstrap-owner
```

Run `git worktree prune`, verify the path/ref are absent, verify unrelated
worktrees remain, and leave `main` clean. Do not push, tag, publish, or alter
remote refs.

## Plan Self-Review

- Every Spec acceptance criterion maps to Tasks 1-4.
- All code symbols and signatures are defined before later use.
- The only behavior change is ownership enforcement; concrete title/draw and
  renderer behavior remain frozen.
- Cold registry migration is excluded rather than approximated by a TUI list
  merge.
- The project rule of one semantic slice commit overrides generic per-step
  commit guidance.
