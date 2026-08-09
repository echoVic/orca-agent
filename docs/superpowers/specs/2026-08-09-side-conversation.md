# Side Conversation

## Problem

During a long-running main conversation, users often need a quick question, a
small comparison, or a non-destructive explanation without changing the main
thread's next prompt. `/fork` cannot provide this: it replaces the active
conversation with a durable fork and requires the current work to be idle.

## Reference implementations and deliberate choices

The interaction is based on source-level inspection of three existing clients:

- Claude Code's `/btw` is a one-shot modal side question. It shares the parent
  context, blocks tools, limits the request to one turn, supports abort/dismiss,
  and never adds the answer to the main transcript.
- Codex's `/side` is a temporary attached child. `Ctrl+/` switches between the
  child and parent without interrupting either one, while `Ctrl+C` discards the
  child. The parent continues to surface actionable status while hidden.
- Grok's `/btw` is a non-blocking overlay. It is explicitly outside the main
  turn, has loading/done/error states, can be dismissed with `Esc`, drops late
  replies after dismissal, and persists only a display record rather than
  changing the main conversation.

Orca adopts Codex's persistent-in-view switching and lifecycle fencing, plus
Claude/Grok's explicit side-question boundary and non-main transcript. Unlike
Claude/Grok's one-shot-only path, Orca keeps the child available for follow-up
turns while it is visible; it remains process-local and disposable. Side tool
use is non-mutating by default and every mutation still passes normal runtime
permission checks.

Side Conversation adds a temporary child surface that can be opened while the
main thread is idle, running, waiting for approval, or waiting for user input.
It is a separate runtime-owned operation and a separate TUI transcript. The
main thread remains the source of truth for the main task and continues to run.

## Scope

### In scope

- Runtime support for creating a side child from an atomic snapshot of a live
  `RuntimeThreadHandle`.
- A process-local, `EphemeralAttached` child surface with a distinct thread
  identity, parent identity, cancellation, and shutdown/join barrier.
- Inherited conversation context copied at the fork boundary, followed by a
  runtime-owned side boundary instruction. Inherited messages are reference
  context; only prompts submitted after the boundary are active instructions.
- TUI actions for `/side [question]`, a configurable global toggle shortcut
  (default `Ctrl+/`), and explicit close/cancel (`Ctrl+C` while Side is active).
- A Side-only transcript view and footer status showing its parent and the
  parent's latest actionable state (`needs input`, `needs approval`,
  `running`, `finished`, `failed`, `interrupted`, or `closed`).
- Explicit cleanup: closing Side cancels it, waits for its terminal result,
  joins its actor, fences late events, and then drops local state. Parent
  replacement/navigation is rejected while a Side child is attached, so no
  hidden child can outlive a replaced parent.
- Runtime and TUI behavior tests, formatter/diff checks, focused tests, full
  workspace tests, and a real terminal smoke check.

### Out of scope

- Persisting, cataloguing, resuming, or independently naming Side threads.
- Merging Side messages, tool results, plans, goals, memory, or settings back
  into the parent. The only handoff is user copy/paste.
- Starting a second agent loop in the TUI or a detached cleanup worker.
- Automatically stopping or transferring the parent's operation when Side is
  opened.
- Allowing Side to mutate files, git state, permissions, configuration, goals,
  memory, or sub-agents by default. A user may explicitly request a mutation;
  normal runtime permissions still apply and the request must remain scoped.

## Lifecycle and ownership

1. `StartSide(parent, prompt?)` is accepted by the runtime host. The parent
   actor supplies one authoritative snapshot; the host creates a child with a
   fresh runtime/surface identity and `parent_thread_id`.
2. The child uses disabled history plus `EphemeralAttached` surface
   persistence. It stays alive for multiple turns while visible and never
   creates a session catalog entry or `memory.md` record.
3. Parent and child have independent operation admission, interaction keys,
   cancellation tokens, event generations, and typed snapshots. Events are
   projected only to the currently attached TUI surface; parent events received
   while Side is visible update the parent-status badge, not the Side
   transcript.
4. Toggle from Side returns to its parent. `Ctrl+C` while Side is active closes
   Side; it does not interrupt the parent. Closing performs cancel, terminal
   settlement, actor join, event unsubscribe/fencing, and state removal in that
   order. If cleanup fails, Side remains visible with an actionable error.
5. If the TUI shuts down, Side is closed through the same barrier before the
   parent handle is released. Parent replacement/navigation is rejected while
   Side is attached; the user closes Side first. A process restart discards
   Side; the durable parent is unaffected.
6. Parent completion/failure/approval/input is observable from Side but never
   silently answered by Side. Side interaction responses are routed only to the
   Side child.

## Interaction design

- `/side` opens an empty Side composer. `/side Explain the last tool error`
  opens Side and submits that question after the boundary is installed.
- `Ctrl+/` toggles between the parent and its Side when one exists. The key is
  configurable through the existing shortcut/keymap mechanism and is shown in
  the footer; it is not overloaded onto interrupt (`Ctrl+C`) or search
  (`Ctrl+F`).
- Side footer: `Side from main · main needs approval · Ctrl+/ to switch · Ctrl+C to close`.
  Parent footer while a Side exists: `Ctrl+/ for side`.
- While a Side child is attached, Side disables destructive/navigation commands
  that could orphan or replace its parent (`/fork`, `/new`, `/resume`,
  `/rename`, `/archive`, `/delete`, `/backtrack`, goal/memory mutation, and
  sub-agent controls), even when the parent projection is visible. The command
  is rejected with a short explanation; press `Ctrl+C` to close Side first.
  Normal submit, cancel, approval, and user input remain available.
- Esc does not backtrack Side history. An empty composer plus the toggle key is
  the unobtrusive return path; close is always explicit and visible.
- Side messages are rendered in their own projection and are never appended to
  the parent transcript. Switching back is lossless for the parent and makes
  the Side disposable by design.

## Compatibility and migration

Existing `/fork` keeps its durable replacement-session behavior. Existing
saved-session picker, resume, and session metadata APIs remain unchanged.
The new runtime request is additive and must reject attempts to combine an
attached ephemeral child with `Record`, `Resume`, or `Fork` history. No legacy
Side implementation exists to migrate; the old TUI-only state path must not be
introduced.

## Acceptance gates

- Runtime tests prove: snapshot-at-cutover, distinct parent/child identities,
  no durable artifact, independent turns, cancellation, close/join, and late
  event fencing.
- TUI tests prove: `/side` parsing, shortcut routing in idle/running/approval
  states, separate transcript projection, parent-status footer, command
  rejection, and close-vs-parent-cancel semantics.
- `cargo test -p orca-runtime side_conversation --lib -- --test-threads=1`
- `cargo test -p orca-tui side_conversation --lib -- --test-threads=1`
- `cargo test -p orca-runtime --lib -- --test-threads=1`
- `cargo test -p orca-tui --lib -- --test-threads=1`
- `cargo test --workspace --all-targets --all-features -- --test-threads=1`
- `cargo fmt --all -- --check` and `git diff --check`
- Real terminal smoke: open a running main turn, press `Ctrl+/`, submit a
  question, observe the Side badge and independent response, return with
  `Ctrl+/`, then close the Side with `Ctrl+C`; verify the main transcript and
  saved-session list are unchanged.
