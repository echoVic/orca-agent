# ThreadActor Surface-Capability Extraction — Implementation Plan

> Slice 1 of the roadmap's "ThreadActor split completion". Behavioral oracle:
> the existing runtime-host lifecycle tests; no behavior change.

**Goal:** Move the ten pure surface-capability commit-batch builders from
`ThreadActor` into `runtime_actor::capability` so the capability module owns
its batch construction, verified by the unchanged behavioral oracle.

## Task 1: Freeze the seam

- [x] Read each of the ten builders and confirm they only read
  `self.resident_surface.coordinator.state().snapshot()` via
  `surface_event_batch_with_commit_id` (plus their parameters).
- [x] Confirm the call sites of each builder inside `runtime_host.rs`
  (grep each name) and their surrounding `&self`/`&mut self` context.

## Task 2: Move the builders into `runtime_actor::capability`

- [x] Add `pub(super) mod batch` (or a `SurfaceCapabilityBatch` namespace in
  `capability.rs`) with the ten functions, each taking the explicit inputs
  plus `snapshot: &surface::SurfaceStateSnapshot`.
- [x] Replace each actor builder with a one-line delegation passing
  `self.resident_surface.coordinator.state().snapshot()`.
- [x] Delete the moved bodies; no logic edits (review the diff for event
  construction changes).

## Task 3: Verify the behavioral oracle

- [x] `cargo test -p orca-runtime --test runtime_host --locked` — 5
  consecutive default-parallelism runs green.
- [x] `cargo test -p orca-runtime --lib --locked` green.
- [x] `cargo nextest run -p orca-runtime --lib --locked --profile ci` green.
- [x] `cargo fmt --all -- --check` and `git diff --check` clean.

## Task 4: Commit and integrate

- [x] One semantic commit: "refactor(runtime): own capability batch
  construction in runtime_actor::capability".
- [x] Rebase onto latest main; re-run the oracle; fast-forward main; push.

## Follow-Up Slices

- [x] Slice 2: settlement sequencing moved into
  `runtime_actor::capability::resolve_capability_commit` with a typed
  `CapabilityCommitStep` (Retained/Deferred/Finished) breaking the
  callback cycle by returning owned data; the actor keeps the deferred
  dispatch and the coordinator commit line.
- [ ] Slices 3-6: ACP read/write/terminal create/observation/cleanup flows
  (the deferred-settlement arms the actor still matches).
