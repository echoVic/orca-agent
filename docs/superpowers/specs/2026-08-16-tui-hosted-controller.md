# TUI Hosted Controller Ownership

Status: Implemented on local `main`

## Context

At audited base `4b13b4afc`, `app.rs` still owns the 511-line
`hosted_tui_controller_loop`. The concrete hosted action transactions have
already moved behind focused owners for Side, session, plan, submission,
workflow, operation recovery, settings, context, tasks, and Goal actions. The
remaining loop now performs four controller responsibilities:

1. establish the initial attachment route and optionally restore a typed saved
   session;
2. receive `UserAction` values and reject main-only commands while Side is
   active;
3. map each accepted action to its existing focused hosted owner; and
4. stop the active Side or main runtime thread on controller exit.

Keeping this lifecycle and dispatch owner at the end of the renderer module
makes `app.rs` depend on every hosted action implementation and leaves the
production controller boundary implicit. The production renderer calls the
loop once from `TuiAgentRuntime::spawn_hosted`; tests call the same loop through
their runtime harness.

## Decision

Add `hosted_controller.rs` as the single owner of the hosted controller loop.
Move the current loop there without changing valid-action branch order,
arguments, event payloads, channel semantics, attachment fencing, runtime
ownership, or shutdown behavior. Expose one crate-private controller entry
point to `app.rs`; keep the renderer responsible for constructing
`TuiAgentRuntime`, terminal/frame state, channels, and the controller inputs.
Reject a malformed empty fallback model instead of panicking in an
`unreachable!` assertion; all production producers already validate or encode
their model action, so valid UI behavior remains unchanged.

This is an ownership extraction, not a new dispatcher, reducer, queue, retry
layer, or runtime abstraction. The existing focused `hosted_*` modules remain
the only owners of their action transactions.

## Frozen Controller Semantics

### Startup And Attachment

1. Start with attachment id `1`, create one shared `AttachmentRouting`, create
   the routed event sender, and publish the same initial attachment switch.
2. Start with no runtime thread and no Side parent.
3. Inspect the configured startup history mode exactly once. Only typed Resume,
   ResumeAt, and Fork modes with no preloaded transcript enter restoration.
4. Load saved-session metadata, ensure the hosted thread, and emit the typed
   history snapshot in the existing order.
5. Preserve the exact fallback empty-history label and error prefix. Continue
   suppressing the duplicate `typed TUI snapshot attachment unavailable`
   error.
6. Announce runtime readiness only when startup created the previously missing
   thread.

### Receive And Dispatch

1. When controller shutdown is already requested, synthesize `Cancel` instead
   of waiting on the action channel. Otherwise block on the existing bounded
   receiver.
2. While Side is active, reject the same main-only action set before any owner
   call with exactly
   `OperationRejected("this command is unavailable in a side conversation; return to main first")`,
   then keep the controller alive.
3. Preserve every existing `UserAction` to hosted-owner mapping and all
   arguments, including SubmittedTurn constructors, active Side config
   selection, settings-intent decoding, and task/Goal commands.
4. Continue treating Interrupt and BackgroundCurrentTurn as controller no-ops;
   their lifecycle remains owned by the dispatcher and task control.
5. Continue ignoring RespondToInteraction in the controller because the
   dispatcher/runtime interaction acknowledgement path owns it.
6. Exit only on Cancel or action-channel disconnect.
7. If a fallback `SetModel` payload is empty after trimming, emit exactly
   `OperationRejected("invalid model selection: Empty")`, keep the current
   config unchanged, and continue receiving actions.

### Exit

1. If Side is active, call the existing attached-Side shutdown helper exactly
   once and do not separately shut down the parent thread.
2. Otherwise, shut down the current runtime thread exactly once when present.
3. Preserve all operation cancellation/join behavior owned by
   `TuiSurfaceTaskControl`, `TuiAgentRuntime`, and runtime thread shutdown.

## Ownership And Compatibility

- `app.rs` retains terminal setup/cleanup, input and frame scheduling,
  `TuiAgentRuntime` construction, initial-prompt presentation, test harnesses,
  and the production call to the controller.
- `hosted_controller.rs` owns only the action receive loop and the lifecycle
  state it coordinates: current thread, optional Side parent, attachment id,
  and attachment routing.
- Existing hosted action modules retain mutation, persistence, retry, timeout,
  event shaping, activation guards, task control, and durable fencing.
- No `UserAction`, `TuiEvent`, runtime surface, CLI/slash syntax, server/JSONL,
  app-server, ACP, transcript, schema, persistence, or public Rust API changes.
- No channel capacity, worker count, timeout, retry, cancellation token, or
  shutdown ordering changes.
- The only malformed-input behavior change is that an empty internal
  `SetModel` action is rejected instead of panicking the controller thread.

## Validator Contract

Move controller-side action and lifecycle anchors from `app.rs` to
`hosted_controller.rs`. Keep `app.rs` anchored as the production caller. Update
the direct mutation baseline so the one runtime-thread shutdown is attributed
to the new owner. Negative self-tests must prove that imports, enum variants,
owner match branches, tests, or the `app.rs` call alone cannot satisfy a missing
production controller mapping.

## Test Strategy

1. Add a direct controller-owner test through the initially absent
   crate-private entry point. Start the real hosted runtime/controller, send
   missing-session StopTask and ForegroundTask actions, assert their exact
   owner errors in FIFO order, then cancel cleanly. This is RED before the
   controller module/API exists and GREEN after extraction; the second action
   proves the loop survives the first rejection and continues dispatching.
2. Add a review-driven RED regression that sends a whitespace-only SetModel,
   reproduces the controller panic, then require the exact rejection and a
   successfully handled follow-up action after the repair.
3. Keep existing startup resume/fork, Side activation/toggle/close, session
   lifecycle, submission, workflow, operation, settings, context, task, Goal,
   cancellation, shutdown, background reentry, and queued-follow-up tests as
   downstream behavioral evidence.
4. Run focused controller and hosted action tests, compiler check, the full
   serial TUI library suite, root-package PTY contract, runtime and Windows
   validators plus self-tests, formatter, and diff checks.
5. Request independent review focused on semantic relocation, receiver and
   shutdown ordering, attachment fencing, mutation ownership, validator
   integrity, and compatibility.

## Acceptance Criteria

1. `hosted_controller.rs` is the only definition of the hosted controller loop;
   `app.rs` is a caller and no longer imports every hosted action owner for
   production dispatch.
2. The direct owner test is RED before the new API and GREEN after extraction,
   proving exact FIFO dispatch across a rejected first action and clean exit.
3. The moved body is semantically identical for valid actions: startup
   restoration, Side restrictions, all action mappings, no-op actions,
   disconnect behavior, and exit shutdown order remain unchanged. Malformed
   empty fallback models take the explicitly frozen rejection path.
4. Runtime-surface validation rejects deletion of production controller
   mappings or shutdown while unrelated textual references remain.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review has no unresolved Critical or Important finding.
7. After local-main integration and root verification, remove only the slice
   worktree and merged topic branch immediately.

## Out Of Scope

- Moving the terminal/frame renderer loop or its test harnesses.
- Changing hosted action payloads, errors, ordering, or runtime mutation
  authority.
- Merging the focused hosted action modules into the controller.
- Cold legacy registry reconciliation or pending-store retirement.

## Implementation Evidence

- The direct owner test first failed with `E0432` because
  `hosted_controller::hosted_tui_controller_loop` did not exist. It passes
  after the extraction and proves exact StopTask then ForegroundTask rejection
  order before clean shutdown.
- The moved function body compares identically with audited base `4b13b4afc`
  after normalizing crate-private visibility and the one review-driven
  malformed-model repair. `app.rs` now imports and calls the owner while its
  production renderer and test harnesses retain their existing runtime
  construction paths.
- Both focused owner tests and 25 existing `hosted_tui_` behavior tests pass.
  The malformed-model test first reproduced the controller-agent panic and
  now proves exact rejection plus a successful follow-up dispatch.
  Compiler check, runtime and Windows validators, validator self-tests, the
  manifest digest, formatter, and diff checks also pass.
- Controller-side typed action and entrypoint anchors now point to
  `hosted_controller.rs`; the production `app.rs` caller has a call-shaped
  anchor that cannot be satisfied by remaining test-harness calls. The one
  direct runtime-thread shutdown baseline moved to the new owner.
- The post-review full serial TUI suite passes 1,112/1,112 and the root-package
  PTY contract passes 6/6.
- CodeRabbit's only Major finding in the changed source identified the
  malformed fallback-model panic; it is resolved by the RED/GREEN regression.
  Its incremental review reports no finding in `hosted_controller.rs`; the
  remaining Major is in unchanged `hosted_side.rs`, and the historical-spec
  Minor explicitly describes its audited base. Its roadmap date Minor is also
  resolved.
- Post-extraction source sizes are `app.rs` 8,319 lines and
  `hosted_controller.rs` 683 lines.
- The topic was already based on latest local `main`, so rebase was a no-op.
  Post-rebase topic verification passed the full serial TUI suite
  1,112/1,112, PTY 6/6, both validators and self-tests, formatter, and diff
  checks.
- After fast-forward integration, local-main verification passed the full
  serial TUI suite 1,112/1,112 in 271.39 seconds and PTY 6/6 in 9.85 seconds,
  followed by both validators and self-tests, formatter, and diff checks. The
  clean slice worktree and merged topic branch were then removed immediately;
  unrelated worktrees were preserved.
