# TUI Attachment Routing Boundary

## Scope

Move TUI attachment relay ownership out of `app.rs`. The routing module owns
attachment activation, stale-event rejection, parent-side status translation,
and deferred parent interaction delivery while a side conversation is active.

## Contract

- every relayed event remains tagged with the source attachment generation;
- an event from an inactive attachment must not mutate the visible `AppState`;
- parent approval/input events are retained while a side conversation is
  active, then delivered in arrival order when the parent becomes visible;
- the app loop owns actor lifecycle and chooses the active attachment, but it
  does not interpret runtime event protocol details.

## Non-Goals

- No change to runtime-surface batches, TUI event payloads, or PTY rendering.
- No migration of unrelated keyboard, compositor, or workflow UI control flow.
