# TUI Side Background Reentry Specification

## Status

Implemented on `codex/tui-side-background-reentry`, based on local `main` at `d039419b8`.

## Problem And Evidence

This is a TUI session-lifecycle and presentation-ownership defect. A Side conversation can background its active typed-surface operation, return to the parent, then become visible again before the operation is terminal. The Side activation path correctly calls `rotate_side_event_sender` before publishing its authoritative `SessionProjectionReset` and `HistoryLoaded` batch. That rotation protects the transcript from hidden Side events, but the already-running background presentation monitor still owns the old sender.

`spawn_background_presentation` captures its `mpsc::Sender<TuiEvent>` at background handoff. `AttachmentRouting` later tags monitor events with the retired attachment. `accept_attached_tui_event` rejects that attachment after Side reentry, so later `WorkflowTasksUpdated` terminal state cannot reach the visible Side panel. The runtime operation continues correctly; its TUI presentation lease is stale.

`foreground_task` is not a remedy. It deliberately transfers the runtime operation from background ownership to a foreground attachment and replays stream output. A Side toggle must not silently undo a user's background choice.

## User Value

Returning to an in-flight Side conversation preserves background semantics while its task panel continues to show authoritative terminal status. The transcript remains fenced from pre-reset events, and explicit foreground control remains available through the existing workflow panel.

## Scope

Add one `surface_client` helper that reads the typed-surface snapshot and replaces the TUI presentation monitor for every live background operation with a monitor bound to the current sender. Invoke it only after parent-to-Side activation has installed the new attachment and delivered the Side reset/history batch.

`TuiSurfaceTaskControl::spawn_surface_presentation` already cancels, joins, and replaces a monitor for the same operation id. The helper therefore creates no concurrent duplicate observer. A snapshot with no background operations is a successful no-op.

## Non-Goals

- Do not foreground, cancel, or replay a background operation.
- Do not weaken attachment fencing or reuse a retired Side attachment.
- Do not change Side creation, parent return, Side close, runtime task persistence, history format, CLI behavior, server/JSONL protocol, or surface-event schemas.

## Ownership And Lifecycle

The runtime surface owns the operation, its background fence, and terminal record. `TuiSurfaceTaskControl` owns at most one presentation monitor per surface operation and joins its predecessor before retaining a replacement. The Side controller owns the active Side attachment and sender. `AttachmentRouting` continues to reject old-generation relay events; only the replacement monitor publishes through the new generation.

Normal path: activate Side, deliver reset/history under the new attachment, then rebind every live background monitor. Cancellation and TUI shutdown use the existing cancellation checks and join path. If reattachment cannot read the live surface, report `OperationRejected` on the active Side sender and retain the completed reset/history switch; runtime ownership is unchanged. A terminal operation is absent from `background_operations`, so it is not retried. No persistent or external protocol data changes; rollback is one commit revert.

## Compatibility

CLI arguments, TUI commands and key bindings, server/JSONL messages, runtime-surface types, and persisted session data are unchanged. The observable difference is that a backgrounded Side task continues publishing authoritative task status after the user returns to Side.

## Behavioral Acceptance

1. A RED test backgrounds a delayed Side operation, toggles Side to parent and back, and fails because the reactivated attachment never receives its terminal task update.
2. The GREEN path receives the terminal `WorkflowTasksUpdated` for the captured Side task id while `is_backgrounded` remains true.
3. Existing stale Side attachment events remain rejected after rotation.
4. Existing background handoff, explicit foreground, Side identity, and PTY workflows remain green.
5. Formatting, diff, and runtime-surface contract validation pass without a manifest or digest change.

## Verification

```bash
cargo test -p orca-tui side_reentry --lib --locked -- --test-threads=1
cargo test -p orca-tui backgrounded --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib app::tests::hosted_side_switches_project_recorded_parent_and_ephemeral_side_identity --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```
