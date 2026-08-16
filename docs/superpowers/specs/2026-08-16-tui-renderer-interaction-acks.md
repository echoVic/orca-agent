# TUI Renderer Interaction Acknowledgement Ownership

Status: Implemented

## Context

At audited base `67eb0127f`, `app.rs` is 7,782 lines. The action dispatcher
owns the bounded interaction-acknowledgement channel and produces one typed
`InteractionResponseAck` for each admitted response. `runtime_event_actions`
owns the effect of one acknowledgement on pending input, composer, Vim, error,
status, and scroll state. The renderer loop still owns the receive side and
batch protocol directly:

1. clone the dispatcher acknowledgement receiver through `TuiAgentRuntime`;
2. non-blockingly drain every acknowledgement queued at the frame boundary;
3. apply each acknowledgement in channel FIFO order through
   `handle_interaction_response_ack`;
4. remember whether at least one acknowledgement was received, even when its
   reducer is intentionally a no-op; and
5. mark the frame dirty once for any non-empty acknowledgement batch.

This is the remaining bridge between dispatcher acknowledgement ownership and
the single-ack presentation reducer. Leaving receiver identity and batch
semantics as renderer locals makes the outer loop responsible for another
small but correctness-sensitive channel lifecycle.

## Decision

Add `renderer_interaction_acks.rs` with one private
`RendererInteractionAckOwner`. It consumes the existing cloned
`Receiver<InteractionResponseAck>` and exposes a non-blocking `drain` method.
`drain` applies all currently queued acknowledgements through the existing
single-ack reducer and returns `true` if it received at least one item.

`app.rs` constructs the owner from the same `TuiAgentRuntime` receiver and
keeps the existing frame-dirty decision: one `mark_dirty()` call when `drain`
returns true. This keeps frame policy out of the acknowledgement owner while
moving receiver identity, iteration, and batch activity ownership out of the
application loop.

This extraction adds no channel, clone, sender, capacity, thread, blocking
wait, retry, wakeup, or acknowledgement variant.

## Frozen Semantics

### Admission And Batch Boundaries

1. The dispatcher remains the only acknowledgement producer and keeps the
   existing bounded capacity of `TUI_EVENT_CAPACITY + USER_ACTION_CAPACITY +
   1`.
2. The owner retains exactly the receiver clone already returned by
   `TuiAgentRuntime::interaction_ack_receiver`.
3. `drain` remains non-blocking and uses current-queue semantics equivalent to
   `try_iter()`: an acknowledgement arriving after the iterator observes an
   empty queue waits for the next renderer iteration.
4. Every acknowledgement already queued is drained in channel FIFO order.
   There is no new per-frame cap because the producer channel is already
   bounded.
5. An empty connected receiver and an empty disconnected receiver both return
   `false`, matching the current `try_iter()` behavior.

### Reduction And Frame Dirtiness

1. Each received acknowledgement is passed exactly once to
   `handle_interaction_response_ack` with the same `AppState`, `TextArea`,
   `VimState`, and `Theme`.
2. `Committed`, `NoLongerPending`, and `Failed` payloads and their reducer
   behavior remain unchanged. The owner does not inspect, reorder, clone, or
   fabricate acknowledgements.
3. `drain` returns `true` for every non-empty batch, including a stale or
   already-committed acknowledgement whose reducer makes no visible change.
4. `app.rs` calls `RendererFrameOwner::mark_dirty` once for a non-empty batch,
   never once per acknowledgement, and never for an empty batch.
5. A reducer panic remains a renderer-thread panic; no new catch, translation,
   or retry boundary is introduced.

## Ownership And Compatibility

- `action_dispatcher.rs` keeps acknowledgement construction, bounded send,
  overflow error emission, disconnect behavior, and producer shutdown.
- `agent_runtime.rs` keeps dispatcher lifetime and exposes the same receiver
  clone before shutdown.
- `renderer_interaction_acks.rs` owns the renderer receiver, non-blocking FIFO
  drain, and non-empty batch result.
- `runtime_event_actions.rs` keeps all single-ack state/composer/Vim/status/
  scroll reduction.
- `renderer_frame.rs` keeps dirty scheduling and every frame policy.
- `app.rs` keeps owner construction, the one dirty decision, input/runtime
  iteration, presentation, terminal cleanup, and shutdown ordering.
- No `TuiEvent`, `UserAction`, `InteractionResponseAck`, runtime surface,
  public Rust API, CLI/slash syntax, server/JSONL, app-server, ACP, history,
  persistence, schema, error text, or visible TUI behavior changes.

## Validator Contract

Add one closed `renderer_interaction_acks` TUI entrypoint with path-specific
app and owner anchors. Negative self-tests must prove that imports, the
dispatcher receiver accessor, low-level reducer tests, or owner unit tests
cannot mask deletion of app construction/delegation, receiver retention,
`try_iter` FIFO draining, one call to the existing reducer, non-empty activity
tracking, or app-owned dirty marking.

## Test Strategy

1. Register a module containing direct tests that import the absent owner and
   require RED with unresolved `RendererInteractionAckOwner`.
2. Prove connected-empty and disconnected-empty drains are inert and return
   false.
3. Prove an unknown committed acknowledgement still returns true while
   leaving state unchanged.
4. Prove multiple queued rejection acknowledgements are reduced FIFO in one
   drain and the next drain is empty.
5. Prove an acknowledgement sent after owner construction is observed, and a
   failed user-input acknowledgement still routes through the existing exact
   composer restoration reducer.
6. Keep dispatcher capacity/overflow/shutdown tests and single-ack reducer
   restoration/stale/FIFO-adjacent tests as downstream evidence.
7. Run compiler checks, both validator families and self-tests, formatter,
   diff check, full serial TUI, and PTY before and after rebase and again on
   integrated local `main`.

## Acceptance Criteria

1. `run_tui_inner` no longer stores the raw acknowledgement receiver or
   directly calls `try_iter`/`handle_interaction_response_ack`.
2. One private owner preserves exact receiver identity, non-blocking FIFO
   drain, reducer collaborators, and non-empty batch result.
3. App still marks the frame dirty exactly once for every non-empty batch.
4. Direct owner tests are RED before implementation and GREEN afterward.
5. Closed validator anchors and deletion self-tests cover production paths
   without broadening mutation or harmless-method baselines.
6. Full TUI and PTY behavior and all public/protocol/persistence contracts
   remain compatible; independent review has no unresolved Critical or
   Important finding.
7. After local-main integration and root verification, remove only this slice
   worktree and merged topic branch immediately.

## Evidence

- Base `67eb0127f` passes the renderer input-routing owner suite 7/7, full
  serial TUI 1,132/1,132, PTY 6/6, runtime and Windows validators/self-tests,
  formatter, digest, and diff checks.
- The audited renderer drain is `app.rs:250,311-324`.
  `runtime_event_actions.rs:19-63` already owns one acknowledgement's effects;
  `action_dispatcher.rs:16-27,52-56,180-195` already owns variants, capacity,
  and production.
- Base source sizes are `app.rs` 7,782 lines,
  `runtime_event_actions.rs` 1,248 lines, `agent_runtime.rs` 262 lines, and
  `action_dispatcher.rs` 815 lines.
- The RED owner suite failed only with unresolved
  `RendererInteractionAckOwner`; after implementation its four direct tests
  pass. They prove inert empty/disconnected drains, non-empty activity for an
  effect-free ack, FIFO all-current-item draining, post-construction receiver
  identity, and exact failed-input restoration through the existing reducer.
- Focused downstream suites pass: `runtime_event_actions` 20/20,
  `action_dispatcher` 11/11, `agent_runtime` 3/3, and `renderer_frame` 6/6.
- The runtime contract validator and deletion self-tests pass after moving the
  existing `RespondToInteraction` consumer anchor from app to the owner and
  adding closed app/owner `renderer_interaction_acks` anchors. Imports, owner
  tests, construction, unrelated dirty calls, and production calls cannot mask
  a deleted path.
- Ordinary and test compiler checks, runtime and Windows validators plus their
  deletion self-tests, formatter, digest equality, and diff checks pass. The
  first full serial TUI gate passes 1,136/1,136 and the PTY contract passes
  6/6.
- CodeRabbit reviewed all 11 staged slice files and reported zero findings.
- The slice is recorded as one semantic commit, initially `07859fed7` before
  evidence-only amendment and local-main rebase.
- Local `main` remained at base `67eb0127f`; `git rebase main` reported the
  topic up to date. Post-rebase owner 4/4, focused downstream 40/40, runtime
  and Windows validators/self-tests, formatter, diff check, full serial TUI
  1,136/1,136, and PTY 6/6 all pass.
- Local `main` fast-forwarded to the single slice commit. Root verification
  passes owner 4/4, runtime and Windows validators/self-tests, formatter, diff
  check, full serial TUI 1,136/1,136, and PTY 6/6. The clean, merged slice
  worktree and topic branch were then removed immediately; unrelated
  worktrees remain intact.
- Implemented source sizes are `app.rs` 7,777 lines and
  `renderer_interaction_acks.rs` 182 lines. Producer and reducer owner sizes
  remain unchanged.

## Out Of Scope

- Changing acknowledgement variants, capacity, overflow policy, producer
  dispatch, runtime-surface response semantics, or single-ack reduction.
- Moving frame dirtiness, input wake/routing, runtime events, frame drawing,
  terminal lifecycle, or shutdown.
- Adding a wakeup channel or blocking acknowledgement wait.
- Cold legacy registry reconciliation, provider, runtime protocol,
  persistence, server, JSONL, ACP, shell, or release work.
