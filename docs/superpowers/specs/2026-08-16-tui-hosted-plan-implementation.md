# TUI Hosted Plan Implementation Ownership

Status: Implemented on `codex/tui-hosted-plan-implementation`

## Context

At audited base `acecd8921`, plan approval selection is already process-local
and the runtime surface owns both live settings and turn admission. The hosted
TUI controller still owns the ordered `ImplementApprovedPlan` transaction
inline in `app.rs`:

- applying the pre-plan approval mode through the hosted settings owner;
- publishing `PlanImplementationStarted` only after that update succeeds;
- submitting the fixed approved-plan prompt through the hosted submission
  owner;
- cancelling the dispatcher-prearmed surface activation when settings fail.

This is the last inline controller transaction that composes settings and turn
admission. The helpers already have the correct mutation owners, but their
ordering and failure gate have no focused hosted owner or direct test.

## Decision

Add a private `hosted_plan.rs` module with a crate-private `HostedPlanAction`
and `handle_hosted_plan_action` entry point. Move only the
`ImplementApprovedPlan` sequencing there. `app.rs` retains `UserAction`
selection, Side/session routing, action-channel lifecycle, and final shutdown.

The process-local command is not a second plan, settings, or operation source.
`apply_hosted_settings_action` remains the only settings transaction owner and
`handle_hosted_submitted_turn` remains the only submitted-turn owner.

## Frozen Ordering

1. Receive the original prompt and target `ApprovalMode` selected by the plan
   approval UI. Do not reinterpret either value.
2. Translate the target mode into exactly one
   `RuntimeSettingsPatch::SetApprovalMode` with `surface_approval_mode`.
3. Call `apply_hosted_settings_action` with the current optional runtime thread,
   shared config, and existing event sender.
4. If settings application returns `false`, cancel the existing
   `TuiSurfaceTaskControl` activation and return. Emit no
   `PlanImplementationStarted` and do not enter submitted-turn handling.
5. If settings application succeeds, its `SettingsUpdated` event remains first.
   Then emit `PlanImplementationStarted` containing a clone of the exact prompt.
6. Submit the original prompt as `SubmittedTurn::user` through
   `handle_hosted_submitted_turn` with the same config, preload, thread slot,
   event sender, task control, workflow-notification queue, and runtime host.
7. Thread startup, runtime-ready publication, admission, provider execution,
   terminal events, desktop notification, and failure shaping remain owned by
   the existing submission/runtime path.

## Failure, Cancellation, And Compatibility

- Attached settings rejection leaves the config and thread installed, publishes
  the existing `OperationRejected`, cancels the prearmed activation, and does
  not claim plan implementation began.
- Unattached settings still update startup config before a missing thread is
  started by the submitted-turn owner, preserving existing behavior.
- Interrupt, cancellation, timeout, retry, disconnect, background ownership,
  restart, and queued-input policy remain unchanged. This slice adds no worker,
  retry, timeout, or cancellation state.
- No plan prompt, approval mode, `UserAction`, `TuiEvent`, runtime surface,
  CLI/slash syntax, server/JSONL, app-server, ACP, transcript, schema,
  persistence, or public API changes.

## Test Strategy

1. Add a direct owner rejection test through the absent module using an active
   sessionless runtime thread. Prove the exact settings rejection, unchanged
   config and installed thread, no implementation/submission event, and released
   prearmed activation.
2. Add a direct owner success test using a recorded mock-provider thread. Prove
   `SettingsUpdated` precedes `PlanImplementationStarted`, which precedes
   runtime-ready, turn-start, and successful terminal events; assert the exact
   mode and canonical user prompt in the committed surface snapshot.
3. Add path-specific controller and owner anchors for
   `ImplementApprovedPlan`, with negative validator self-tests that cannot pass
   from imports, enum variants, or tests.
4. Run focused plan/settings/submission tests, compiler check, full serial TUI,
   PTY, runtime/Windows validators and self-tests, formatter, and diff checks.
5. Request independent review focused on settings-before-start ordering,
   prompt/mode identity, activation rollback, downstream ownership, validator
   integrity, and external compatibility.

## Acceptance Criteria

1. Plan implementation has one transaction owner in `hosted_plan.rs`;
   `app.rs` only maps the existing user action.
2. The direct rejection owner test is RED before the owner API exists; direct
   rejection and success tests are GREEN afterward.
3. Settings rejection cannot publish implementation-start or submit the prompt,
   and it releases the prearmed activation.
4. Existing plan selection, settings, submission, interrupt, and PTY behavior
   passes unchanged.
5. Contract validation rejects deletion of the production controller mapping or
   owner branch while other textual references remain.
6. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
7. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `acecd8921`, the direct owner test failed because
  `crate::hosted_plan` did not exist. The focused owner test passes after the
  module and handler were added.
- `hosted_plan.rs` now owns approval patching, settings-success gating,
  implementation-start publication, submitted-turn delegation, and settings
  failure activation rollback. The controller branch contains only the
  crate-private command mapping.
- The direct rejection test uses a real active sessionless runtime thread to
  prove the exact settings rejection, unchanged config and installed thread,
  absence of later events, and released prearmed activation. The direct success
  test uses a recorded mock-provider thread to prove exact mode/prompt
  propagation and `SettingsUpdated` -> `PlanImplementationStarted` ->
  runtime-ready -> turn-start -> successful-terminal ordering, including the
  committed canonical prompt in the typed surface snapshot.
- Three plan-approval selection tests, five hosted-settings tests, and three
  hosted-submission tests pass unchanged. Path-specific controller and owner
  anchors plus their negative validator self-tests pass. Post-extraction source
  sizes are `app.rs` 8,808 lines and `hosted_plan.rs` 247 lines.
- Independent review found no remaining Critical, Important, or Minor issue
  after the direct success regression was added, and approved the slice for
  merge. The reviewer independently reran both owner tests, compiler check,
  both validators and self-tests, formatter, and diff checks.

## Residual Boundary

Task stop/foreground and background interaction control remain controller-owned
action families. Cold legacy registry reconciliation remains an independent
migration boundary.
