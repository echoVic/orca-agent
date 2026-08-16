# TUI Renderer Runtime Event Ownership

Status: Implemented

## Context

At audited base `12646b97b`, `app.rs` is 8,319 lines and its 562-line
`run_tui_inner` still owns a mixed runtime-event branch inside the terminal
frame loop. That branch currently coordinates six renderer-local concerns:

1. attachment admission before any event-specific side effect;
2. typed history hydration followed by the deferred initial prompt;
3. mention search result and catalog refresh events;
4. installation of the active typed surface for mention discovery;
5. startup-history mode and settings mirroring into renderer configuration;
6. delegation of all admitted events to `handle_runtime_event`.

The same function separately owns the per-iteration mention/composer sync and
the mention-search shutdown call. Runtime mutation already belongs to the
typed surface and focused event reducers; the remaining code is renderer-owned
coordination, but its lifecycle and ordering are implicit in the terminal
setup function.

## Decision

Add `renderer_runtime.rs` with one private `RendererRuntimeEventOwner`. The
owner holds the existing `MentionSearchManager` and the optional deferred
initial prompt. It admits and routes one runtime event, synchronizes mention
search from the current composer after each event-loop iteration, and shuts
down mention search when the renderer exits.

`app.rs` continues to own terminal creation and cleanup, input batching, frame
scheduling and drawing, `AppState`, composer/Vim state, runtime construction,
channels, and the loop exit decision. `runtime_event_actions.rs` remains the
only owner of admitted-event reduction and presentation effects.

This is an ownership extraction. It does not add a second event queue,
reducer, projection cache, retry policy, or runtime abstraction.

## Frozen Event Semantics

### Attachment Admission

1. Every runtime event first passes through `accept_attached_tui_event`.
2. `SessionAttachmentActivated` updates the active attachment and produces no
   other reducer, mention, config, prompt, or presentation effect.
3. An event carrying a stale non-active attachment is rejected before every
   other side effect; the deferred initial prompt remains available for a later
   admitted history event.
4. Unattached events retain their current compatibility behavior.

### History And Initial Prompt

1. An admitted `HistoryLoaded` is fully delegated to `handle_runtime_event`
   before the deferred initial prompt is inspected.
2. The first admitted `HistoryLoaded` consumes the optional prompt, appends the
   same optimistic user message, enters running state, and sends the same
   `UserAction::Submit` value.
3. Later history events cannot resubmit the consumed prompt.
4. A stale attached history event cannot consume or submit the prompt.

### Mention Coordination

1. `MentionSearchDirty` reads the current composer text and byte cursor, then
   consumes only the matching search generation.
2. `MentionCatalogDirty` consumes only the matching catalog generation.
3. `MentionRuntimeReady` installs the same typed `TuiSurfaceActions` and keeps
   asynchronous catalog discovery behavior unchanged.
4. After each event-loop iteration, roots, visible text, cursor, mention
   bindings, atomic skill tokens, enabled status, and current time are applied
   in the same order as today.
5. Renderer exit calls the existing bounded/non-blocking mention-search
   shutdown path exactly once.

### Config And General Events

1. `NewSessionStarted` changes renderer `history_mode` to `Record` before the
   event is delegated to `handle_runtime_event`.
2. `SettingsUpdated` mirrors model, reasoning effort, and approval mode into
   renderer `RunConfig` before delegating the unchanged event.
3. Every other admitted event is delegated exactly once with the same
   `AppState`, action sender, pending workflow notifications, composer, Vim
   state, theme, and terminal presentation.

## Ownership And Compatibility

- `renderer_runtime.rs` owns only renderer-side runtime-event admission,
  special-event coordination, deferred initial-prompt consumption, mention
  synchronization, and mention-search shutdown.
- `runtime_event_actions.rs` keeps reducer, workflow notification,
  auto-approval, composer restoration, terminal notification, and queued-input
  behavior.
- `attachment_routing.rs` keeps attachment ids, routing state, relay threads,
  and acceptance rules.
- `mention_search_manager.rs` keeps search sessions, catalog workers,
  generations, cancellation, and projection behavior.
- No `TuiEvent`, `UserAction`, runtime surface, CLI/slash syntax, server/JSONL,
  app-server, ACP, transcript, schema, persistence, or public Rust API changes.
- No channel capacity, event batch limit, frame interval, timeout, worker,
  cancellation, or terminal cleanup order changes.

## Validator Contract

Move the renderer-owned mention-search shutdown mutation attribution from
`app.rs` to `renderer_runtime.rs`. Add path-specific anchors for attachment
admission, deferred prompt consumption, and the general
`handle_runtime_event` delegation. Negative self-tests must prove that imports,
tests, enum variants, or an owner type without the production call paths cannot
satisfy the boundary.

## Test Strategy

1. Add a direct owner test through the initially absent module/API. Activate
   one attachment, send stale attached history, then admitted history twice.
   Assert the stale event does not consume the prompt, the admitted history is
   reduced before prompt submission, and exactly one `Submit` is sent.
2. Add a direct settings test proving renderer config and `AppState` observe the
   same admitted `SettingsUpdated` payload while a stale attached update
   changes neither.
3. Keep existing attachment routing, history/startup, mention search, runtime
   event, settings, interaction acknowledgement, event-loop fairness,
   terminal lifecycle, and PTY tests as downstream evidence.
4. Run focused owner and affected suites, compiler check, full serial TUI
   library suite, root-package PTY contract, runtime and Windows validators
   plus self-tests, formatter, and diff checks.
5. Request independent review focused on admission ordering, prompt exactly-once
   behavior, config mirroring, mention worker shutdown, dependency direction,
   validator integrity, and compatibility.

## Acceptance Criteria

1. `renderer_runtime.rs` is the only production owner of the six special
   runtime-event branches, deferred initial prompt, mention/composer sync, and
   mention-search shutdown.
2. The direct owner test is RED before the module exists and GREEN after the
   extraction, proving stale rejection and exactly-once prompt submission.
3. The moved behavior is semantically identical and all focused, full TUI, PTY,
   validator, formatter, and diff gates pass after rebase and on integrated
   local `main`.
4. Independent review has no unresolved Critical or Important finding.
5. After local-main integration and root verification, remove only the slice
   worktree and merged topic branch immediately.

## Implementation Evidence

- The direct owner suite first failed with `E0432` because
  `RendererRuntimeEventOwner` did not exist. After implementation, both owner
  tests pass and prove stale attachment rejection, admitted history reduction
  before exactly one initial-prompt submission, and stale/admitted settings
  mirroring behavior.
- `app.rs` now constructs the owner and lends renderer state for each runtime
  event and composer sync. The moved branch, sync order, and shutdown call are
  semantically identical to audited base `12646b97b`; terminal, frame,
  dispatcher, and reducer ownership did not move.
- Focused attachment routing (8), mention search (13), runtime event actions
  (20), and resumed typed-history/initial-turn (1) tests pass. The owner suite
  passes 2/2; both test and ordinary `cargo check -p orca-tui --locked`
  configurations pass.
- The runtime-surface manifest now records both the app caller and owner path.
  Negative self-tests delete the caller, attachment admission, deferred prompt
  consumption, and general reducer delegation independently while preserving
  masking imports, fields, or tests; the validator and self-tests pass.
- Post-extraction source sizes are `app.rs` 8,237 lines and
  `renderer_runtime.rs` 395 lines.
- The full serial TUI suite passes 1,114/1,114 and the root-package PTY
  contract passes 6/6. The first full run exposed the missing Rust-side closed
  inventory mirror; the first PTY build exposed a test-only import mistake.
  Both gates were repaired and rerun successfully.
- After the no-op rebase onto current local `main`, the owner suite passes 2/2,
  the full serial TUI suite passes 1,114/1,114 in 42.22 seconds, the PTY
  contract passes 6/6 in 11.73 seconds, and both contract validators plus
  self-tests, formatter, and diff checks pass.
- After fast-forward integration, the root full serial TUI suite passes
  1,114/1,114 in 269.47 seconds and the root PTY contract passes 6/6 in 9.62
  seconds. Both validators and self-tests, formatter, diff check, and manifest
  digest check pass. The clean slice worktree and merged topic branch were
  removed immediately; unrelated worktrees were preserved.
- CodeRabbit reported no Critical or Important finding. Its only Minor found a
  one-line manifest reference drift; the reference and reviewed digest were
  corrected.

## Out Of Scope

- Moving terminal setup/cleanup, input routing, frame scheduling, drawing, or
  the hosted controller.
- Changing runtime-event payloads, reducers, attachment routing, mention search
  algorithms, initial-prompt eligibility, or submission admission.
- Cold legacy registry reconciliation or pending-store retirement.
