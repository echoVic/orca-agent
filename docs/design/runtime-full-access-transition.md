# Runtime Full Access Transition

## Status

Implemented for v0.4.28.

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

The following TUI paths use the same confirmation owner:

- Shift+Tab mode cycling;
- `/mode full-auto`;
- the `/mode` submenu;
- the `/config` settings panel.

The confirmation defaults to Cancel. A pending `/config` change keeps its model
and reasoning choices, but none of those changes are submitted if the user
cancels Full Access.

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
- Unconfirmed Full Access intents and generic widening patches fail closed.
- An active operation observes `Workspace` before the commit and
  `TrustedHost` / `DangerFullAccess` after it.
- A retained pre-commit policy snapshot remains unchanged.
- New delegation snapshots inherit `TrustedHost`; existing snapshots remain
  unchanged.
- Session metadata can explicitly clear a previously persisted permission
  profile.
