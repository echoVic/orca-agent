# ThreadActor Surface-Capability Extraction — Implementation Plan

> Slice 1 of the roadmap's "ThreadActor split completion". Behavioral oracle:
> the existing runtime-host lifecycle tests; no behavior change.

**Goal:** Move the ten pure surface-capability commit-batch builders from
`ThreadActor` into `runtime_actor::capability` so the capability module owns
its batch construction, verified by the unchanged behavioral oracle.

## Task 1: Freeze the seam

- [ ] Read each of the ten builders and confirm they only read
  `self.resident_surface.coordinator.state().snapshot()` via
  `surface_event_batch_with_commit_id` (plus their parameters).
- [ ] Confirm the call sites of each builder inside `runtime_host.rs`
  (grep each name) and their surrounding `&self`/`&mut self` context.

## Task 2: Move the builders into `runtime_actor::capability`

- [ ] Add `pub(super) mod batch` (or a `SurfaceCapabilityBatch` namespace in
  `capability.rs`) with the ten functions, each taking the explicit inputs
  plus `snapshot: &surface::SurfaceStateSnapshot`.
- [ ] Replace each actor builder with a one-line delegation passing
  `self.resident_surface.coordinator.state().snapshot()`.
- [ ] Delete the moved bodies; no logic edits (review the diff for event
  construction changes).

## Task 3: Verify the behavioral oracle

- [ ] `cargo test -p orca-runtime --test runtime_host --locked` — 5
  consecutive default-parallelism runs green.
- [ ] `cargo test -p orca-runtime --lib --locked` green.
- [ ] `cargo nextest run -p orca-runtime --lib --locked --profile ci` green.
- [ ] `cargo fmt --all -- --check` and `git diff --check` clean.

## Task 4: Commit and integrate

- [ ] One semantic commit: "refactor(runtime): own capability batch
  construction in runtime_actor::capability".
- [ ] Rebase onto latest main; re-run the oracle; fast-forward main; push.

## Follow-Up Slices (not in this commit)

- Slice 2: settlement orchestration (`retry/apply/deferred`) behind an
  injected dispatcher to break the `settle_*` callback cycle.
- Slices 3-6: ACP read/write/terminal create/observation/cleanup flows.
