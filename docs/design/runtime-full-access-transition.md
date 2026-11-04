# Runtime Full Access Transition

## Status

Implemented for v0.4.29.

## Contract

`full-auto` is a real execution-authority change:

- approval mode becomes `FullAuto`;
- execution profile becomes `TrustedHost`;
- the default shell sandbox becomes `DangerFullAccess`;
- an active permission profile is cleared so it cannot override Full Access.

The TUI must not display `full-auto` until the runtime has committed the
settings mutation. Moving from another mode to `full-auto` requires an explicit
confirmation. Canceling the confirmation sends no runtime mutation.

## Runtime Boundary

Interactive authority widening during an active operation uses the dedicated
`RuntimeSettingsPatch::EnableFullAccess` variant. A generic
`SetApprovalMode(FullAuto)` cannot widen an active operation. This keeps TUI
confirmation provenance distinct from ordinary mode synchronization and
server-side configuration.

The runtime commits the surface settings revision and policy epoch before it
publishes the new operation policy or acknowledges the mutation.

Each active operation owns a `RuntimeExecutionPolicyHandle`. Every tool
dispatch captures one immutable `RunConfig` snapshot from that handle:

- an already admitted tool retains the snapshot it started with;
- the next tool admission observes the newly committed Full Access policy;
- a new child delegation captures the new `TrustedHost` policy;
- an existing delegated child retains its immutable `DelegationSnapshot`.

This permits a user to widen the current operation deliberately without
allowing a running tool or child to gain authority retroactively.

## Entry Points

New standalone TUI sessions use `FullAuto` / `TrustedHost` as their built-in
fallback, below explicit CLI, environment, and user configuration choices. This
default does not populate `startup_approval_mode`, so session recovery keeps its
saved mode. Session metadata seeds the mode and active profile for older histories
without typed settings; typed settings replace that seed during replay. An explicit
`--yolo` or `--mode` remains a launch override applied after recovery. Shared daemon, protocol, and headless defaults stay `AutoEdit`.

The following TUI paths use the same confirmation owner:

- Shift+Tab mode cycling;
- `/mode full-auto`;
- the `/mode` submenu;
- the `/config` settings panel.

The confirmation defaults to Cancel. A pending `/config` change keeps its model
and reasoning choices, but none of those changes are submitted if the user
cancels Full Access.

While a tool or permission request is awaiting approval, Shift+Tab opens this
confirmation directly from Suggest, AutoEdit, or Plan. It does nothing if the
mode is already FullAuto. The hosted action dispatcher applies a confirmed
`EnableFullAccess` through the typed settings surface while the turn is active,
rather than queuing the mutation behind that turn. It mirrors committed
settings into the shared configuration and publishes the result through the
current attachment. Side conversations and focused children retain their
existing settings restrictions.

Settings intents carry their dispatcher order and target thread. A successful
Full Access commit supersedes older queued mode changes for that thread while
preserving their model and reasoning changes. Later explicit mode choices
remain valid. Both hosted settings paths serialize runtime commit, shared
configuration synchronization, and event publication with a dedicated settings
lock; they do not hold the hosted state or configuration lock during runtime
calls. Session installation and runtime binding share that ordering boundary.
Installation retires the old settings target before the new attachment is
announced, and a settings request rechecks its target after acquiring the lock.
An old captured handle cannot write settings into a replacement session.

Opening, canceling, or confirming Full Access does not answer an already
pending interaction. That request retains its admission policy and must still
be answered once. A rejected active settings change reports an error without
resetting the waiting turn to Idle.

ACP translates an allowed `full-auto` selection to `EnableFullAccess`. The
JSONL/server startup adapter does the same for unprofiled `full-auto`, but
synchronizes a profiled `full-auto` mode and its explicit profile separately.
Generic mode/profile synchronization cannot enter unprofiled `FullAuto` or
widen an operation that is already active. Restoring a no-profile `full-auto`
session normalizes it to `TrustedHost`, while a legacy explicit profile remains
authoritative.

## Diagnostics

`/status` reports:

- approval mode;
- execution profile;
- resolved shell sandbox;
- active permission profile.

Sandbox launch and denial errors include the same resolved policy context, so
backend probe failures cannot be mistaken for a successful Full Access
transition.

## Verification

- TUI confirmation opens from every mode-selection path and defaults to Cancel.
- Cancel sends no mutation; confirm emits `EnableFullAccess`.
- Approval waits support direct confirmation from Suggest, AutoEdit, and Plan;
  default Enter and Esc leave the pending request untouched.
- A hosted turn can commit Full Access before its pending approval is answered,
  then resume that same turn after the user answers.
- Rejected active Full Access leaves the pending approval visible and answerable.
- Session installation waits for settings publication, and a captured request
  for a replaced session is rejected before committing or mirroring settings.
- Unconfirmed Full Access intents and generic widening patches fail closed.
- An active operation observes `Workspace` before the commit and
  `TrustedHost` / `DangerFullAccess` after it.
- A retained pre-commit policy snapshot remains unchanged.
- New delegation snapshots inherit `TrustedHost`; existing snapshots remain
  unchanged.
- Session metadata can explicitly clear a previously persisted permission
  profile.
