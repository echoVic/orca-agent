# ThreadActor Surface-Capability Extraction

## Status

Proposed for `codex/threadactor-capability-extraction`, based on `main` at
`9d1541ffc` (v0.3.16 + test-infra hardening).

## Problem And Evidence (fresh, 2026-08-13)

`crates/orca-runtime/src/runtime_host.rs` is 51,545 lines. `impl ThreadActor`
spans lines 13,513-37,1xx and still mixes at least eight state-machine
categories. The 2026-08-10 evidence-based roadmap ranks this Critical:
"Every new feature therefore continues to increase the largest structural
risk in the repository." The four prior controller extractions
(`runtime_actor::{background, capability, commit, goal, thread_state}`,
2,930 lines total) did not reach the ~8k-line target.

The surface-capability category alone spans roughly 2,100 lines inside the
actor (batch builders, commit settlement orchestration, deferred settlement
dispatch, and the ACP read/write/terminal create/observation/cleanup flows),
while `runtime_actor::capability` (962 lines) already owns the capability
controller (`retry_transition_effect`, `resolve_commit_effect`,
`try_claim_write`, `take_call_with_waiter`, `transitions_empty`,
`pending_transition_ids`). The actor methods call into that controller and
back into other actor methods (`apply_deferred_surface_capability_settlement`
dispatches into eight actor `settle_*` flows), so the category has a coherent
single owner but no module boundary: every change risks touching unrelated
actor state.

Classification: architecture defect (ownership concentration), not a
behavioral defect.

## Value

- Architecture: one bounded module owns capability batch construction and
  settlement; future ACP capability work stops growing the god object.
- TUI/server reliability: unchanged behavior, but reviewable in isolation —
  the extraction is verified by the existing runtime-host lifecycle tests,
  which are the behavioral oracle (no source-shape assertions).
- The roadmap's slice 2 (ThreadActor split completion) progresses through
  its first vertical slice without changing any user-visible behavior.

## Scope (Slice 1)

Move the pure surface-capability commit-batch builders out of `ThreadActor`
into `runtime_actor::capability` as a `SurfaceCapabilityBatch` namespace:

- `capability_call_batch`, `capability_call_transition_batch`
- `ambiguous_capability_tool_events`, `ambiguous_write_capability_batch`
- `terminal_create_completed_batch`, `ambiguous_terminal_create_capability_batch`
- `terminal_cleanup_started_batch`, `terminal_release_started_batch`
- `terminal_cleanup_completed_batch`, `ambiguous_terminal_cleanup_capability_batch`

Each builder becomes a module function taking the inputs it actually reads
(the capability call/terminal ids it already receives) plus a
`&surface::SurfaceStateSnapshot` (the one value `surface_event_batch_with_commit_id`
reads from `self.resident_surface.coordinator`); it returns
`surface::SurfaceCommitBatch` or `Result<_, SurfaceClientCommandError>`
exactly as today. `ThreadActor` keeps thin one-line delegations for the call
sites that pass `self` (changed to pass the snapshot) — the callers inside
the actor keep their shape; only the construction site moves.

## Non-Goals (this slice)

- NOT moving the settlement orchestration (`retry_surface_capability_transition`,
  `apply_surface_capability_commit`,
  `apply_deferred_surface_capability_settlement`,
  `settle_surface_capability_transitions_for_shutdown`) — they need the
  deferred-settlement dispatch cycle broken first; that is slice 2.
- NOT moving the ACP `request_/authorize_/claim_/mark_/settle_*` flows
  (slices 3-6).
- No wire protocol, surface snapshot, persistence, CLI, or TUI change.
- No source-line-count assertion and no splitting of unrelated actor methods
  (the prior spec's non-goal still applies).

## Ownership And Boundaries

- `runtime_actor::capability` owns capability commit-batch construction and
  (already) the capability commit controller. `ThreadActor` keeps command
  dispatch, generation commit sequencing, and the settlement callbacks that
  require actor-owned state (goal recovery, terminal release), which slice 2
  will re-own via an injected dispatcher.
- The surface coordinator remains the single source of surface state; the
  builders read only its snapshot, never actor fields.

## Semantics

Pure move: normal completion, cancellation, rejection, timeout, retry,
disconnect, and restart semantics are unchanged. Every builder keeps its
exact event sequence and commit-id behavior; the existing runtime-host
lifecycle tests (the behavioral oracle) must stay green without edits.

## Compatibility

No CLI, TUI, server/JSONL, or persisted-format change. Internal module
visibility only (`pub(super)` within `runtime_actor`, callers in
`runtime_host` keep using the actor delegations). No public Rust symbol
moves.

## Acceptance Criteria

1. All ten builders live in `runtime_actor::capability` and compile without
   the actor reading capability-batch construction logic inline.
2. The complete existing behavioral oracle passes unchanged:
   `cargo test -p orca-runtime --test runtime_host --locked`
   (default parallelism, 5 consecutive runs),
   `cargo test -p orca-runtime --lib --locked`, and the
   `cargo nextest run -p orca-runtime --lib --locked --profile ci` gate.
3. `cargo fmt --all -- --check` and `git diff --check` clean.
4. The moved code contains no behavioral edits: review confirms the diff is
   relocation plus the snapshot parameter (no changed event construction).

## Verification Commands

```bash
cargo test -p orca-runtime --test runtime_host --locked
cargo test -p orca-runtime --lib --locked
cargo nextest run -p orca-runtime --lib --locked --profile ci
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Slice 1 is a single revertible commit with no persisted migration. The
actor delegations are deleted in the same commit as the module move (no
temporary second path). Slices 2-6 (settlement orchestration + ACP flows)
follow on the same branch with their own specs; the branch is not merged
until each slice's oracle passes.

## Self-Audit

- No TBD/TODO/placeholders; every section is concrete.
- Acceptance criteria map 1:1 to the verification commands.
- Ownership is explicit: capability module owns construction; actor keeps
  dispatch; coordinator remains the state source of truth.
- Slice is independently implementable, verifiable, committable, and
  revertible; no new compatibility layer or second fact source.

## Slice 2: Settlement Orchestration Behind An Injected Dispatcher

### Scope

Move the capability commit settlement orchestration out of `ThreadActor`
into `runtime_actor::capability` as one `SurfaceCapabilitySettlement`
struct owning `&mut` access to the capability controller and the surface
coordinator:

- `retry_surface_capability_transition`
- `apply_surface_capability_commit` (returns the optional
  `RuntimeActorEffect` reply for the actor to apply — actor-effect
  application stays in the actor)
- `apply_deferred_surface_capability_settlement` (deferred variants call
  the eight actor flows through a new
  `DeferredCapabilitySettlementDispatcher` trait implemented by
  `ThreadActor`, breaking the callback cycle)
- `settle_surface_capability_transitions_for_shutdown`

### Non-Goals

- The eight ACP `settle_*`/`begin_*`/dispatch flows stay in the actor
  (slices 3-6 move them).
- No wire, snapshot, persistence, CLI, or TUI change; no behavior change.

### Ownership

- `runtime_actor::capability` owns settlement orchestration; `ThreadActor`
  keeps command dispatch and the actor-owned settle flows, exposed through
  the dispatcher trait.
- The trait methods are typed (no dynamic dispatch objects); one
  implementation exists.

### Acceptance

1. The four orchestration methods live in capability.rs; the actor
   implements the dispatcher trait; compile is clean.
2. Behavioral oracle unchanged: runtime_host integration 66/66 x 5
   consecutive runs, lib suite green, nextest ci lib gate green, fmt and
   diff-check clean.
3. Diff review: relocation plus the trait/parameter plumbing only; no
   event-construction or control-flow edits.
