# TUI Convergence Slice 25: Hosted Settings Ownership

## Status

Implemented on `codex/tui-hosted-settings`, based on clean local `main` at
`a6c15c898`. This slice moves the existing hosted settings translation and
application helpers behind one crate-private owner. It does not change the
runtime surface schema, persisted history, CLI, server/JSONL, ACP, or
user-visible settings behavior.

## Problem And Evidence

The hosted controller delegates runtime mutations to typed surface actions,
but `app.rs` still owns the complete settings transaction below the controller
loop: translating a `SettingsIntent` into ordered runtime patches, mapping the
approval-mode enum, committing patches through an attached runtime thread,
mirroring committed effective settings into `RunConfig`, applying startup-only
settings without a thread, and publishing `SettingsUpdated` or rejection
events.

This is one cohesive mutation boundary with two explicitly different
authorities: an attached runtime snapshot is authoritative for a live thread,
while the shared startup config is authoritative before a thread exists.
Keeping both branches in `app.rs` obscures that distinction and leaves the
controller responsible for patch translation and commit-result shaping. The
pre-extraction `app.rs` was 9,842 lines; after extraction, `app.rs` is 9,674
lines and `hosted_settings.rs` is 350 lines.

## Scope

Move these helpers into `hosted_settings.rs`:

- `settings_intent_patches`;
- `surface_approval_mode`; and
- `apply_hosted_settings_action`.

Keep settings action selection, plan-implementation sequencing, controller
routing, thread/attachment ownership, Side policy, and session lifecycle in
`app.rs`. The new module receives the optional runtime thread, shared config,
event sender, and patches explicitly; it owns no global or background state.

## Non-Goals

- Do not change accepted models, reasoning efforts, approval modes, patch
  order, or empty-patch behavior.
- Do not change runtime settings revision checks, rejection text, projection
  events, or plan-implementation gating.
- Do not make unattached settings durable session facts or add a second
  settings cache.
- Do not change attachment, cancellation, timeout, retry, disconnect, or
  restart policy.
- Do not change runtime surface, persistence, protocol, or public APIs.

## Ownership And Semantics

`hosted_settings` owns translation and application of the existing hosted
settings transaction.

- Intent translation still emits patches in model, reasoning, approval order;
  absent values and invalid/empty model text do not create patches.
- With an attached runtime thread, settings still commit through
  `TuiSurfaceActions::update_settings`. Only a committed effective settings
  snapshot updates the shared config and emits `SettingsUpdated`.
- Runtime update failure still emits `OperationRejected` and returns `false`.
  The existing unsupported-medium result path also rejects without locally
  mirroring the returned snapshot.
- Without a runtime thread, supported patches still update the shared startup
  config directly, unsupported/future patches are ignored, `SettingsUpdated`
  reflects the resulting config, and the helper returns `true`.
- An empty patch vector still returns `false` and emits no event. The controller
  continues to use that result to gate `PlanImplementationStarted` and turn
  submission.

## Acceptance

1. The three helpers have one module owner and no duplicate body remains in
   `app.rs`.
2. Focused tests prove ordered intent translation, empty-patch rejection, the
   unattached local settings/event result, a live recorded-thread
   commit/mirror/event result, and sessionless attached rejection without a
   local mirror, without asserting source shape.
3. Existing plan approval, slash-menu, settings transition, ordinary-turn,
   Side, restart, and PTY behavior remains green.
4. Full TUI, PTY, validators, formatter, and diff checks pass.
5. Independent review finds no changed mutation authority, patch order,
   rejection/event behavior, plan gating, persistence, or public API behavior.

## Evidence

- RED: the hosted-settings filter failed because both new behavior tests found
  only the inaccessible legacy implementations in `app.rs`.
- GREEN: five hosted-settings tests pass for patch order, empty-patch rejection,
  unattached config application, attached typed-runtime commit/mirroring, and
  sessionless attached rejection without local mutation.
- Slash-menu settings selection, AppState settings transition, and three plan
  approval tests pass.
- Full serial TUI suite: 1,080 passed on the final current tree; the five-test
  focused owner filter passes.
- Root PTY suite: 6 passed.
- Runtime-surface and Windows boundary validators plus their self-tests passed.
- Formatter, diff, and `cargo check -p orca-tui --tests --locked` pass.
- Independent review found no Critical or Important behavior, ownership,
  protocol, persistence, or public-API findings. Rebase and integrated-root
  repetition remain the closing gates.

## Verification Commands

```bash
cargo test -p orca-tui hosted_settings --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::slash_submenu_model_flow_asks_for_reasoning_effort_then_applies_both --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui types::tests::settings_transition_remembers_and_restores_pre_plan_mode --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Use one isolated worktree and one semantic commit. Reverting the commit restores
the helper locations without data migration or protocol work.

## Self-Review

The boundary stops before submitted-turn dispatch and latest-active Goal
recovery. Those paths combine runtime-thread lifecycle with operation control
and require separate behavior evidence.
