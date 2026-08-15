# TUI Session Identity Projection Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cursor-fenced runtime surface snapshots the only production source for the TUI's optional recorded session id and authoritative title.

**Architecture:** `SurfaceSessionProjectionState` gates every projection envelope by thread identity, incarnation, and sequence before any session-scoped snapshot owner is updated. `SessionProjectionReset` carries a complete authoritative snapshot and is the sole cross-thread admission path. Rename and fork acknowledgements become once-per-cursor presentation directives, while payload-free `NewSessionStarted` retains only local composer/history-mode behavior.

**Tech Stack:** Rust, `orca-runtime` typed surface snapshots, Ratatui TUI state, Cargo behavior tests, Node runtime-surface contract validator.

---

### Task 1: Prove The Identity Boundary Failures

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs:1620-1960`
- Modify: `crates/orca-tui/src/types.rs:3180-3280`
- Modify: `crates/orca-tui/src/types.rs:4740-5130`
- Modify: `crates/orca-tui/src/app.rs:3600-3940`

- [x] **Step 1: Add a disabled-history identity RED test**

Create a real `SurfaceSnapshot` fixture with
`ThreadPersistence::EphemeralNonCataloguedOneShot` and apply its projection to
AppState. Against the current field API, assert that the title is present but
`current_session_id` is `None`; migrate the same assertion to the immutable
query when Task 2 introduces it. Name the test
`surface_session_projection_does_not_invent_ephemeral_session_id`.

- [x] **Step 2: Add cursor-fence RED coverage**

Add `surface_session_projection_fences_stale_and_cross_thread_identity` with
deterministic cursors. Apply a title at sequence 2, then a stale title at
sequence 1 and a contradictory title at sequence 2; both must preserve the
sequence-2 title. Apply a different thread without reset and assert rejection,
then apply it through `SessionProjectionReset` and assert acceptance.

- [x] **Step 3: Require authoritative rename and fork projections**

Change the existing hosted rename test to fail if `SessionRenamed` arrives
before an authoritative `SurfaceProjectionSynced` carrying the committed
title. Change the fork test to inspect the current string-bearing
`SessionProjectionReset` and require the fork's authoritative title instead of
the placeholder. These assertions compile against the pre-slice types and fail
on behavior, not on missing future fields. In Tasks 2-4, migrate them to require
`session_presentation` respectively `Renamed` or `Forked`, apply the event to
AppState, and assert the immutable id/title queries plus the existing visible
acknowledgement.

- [x] **Step 4: Require a reset snapshot for new and resume**

Change the hosted new-session test to fail when `NewSessionStarted` arrives
before any reset, and change the saved-session resume/fork tests to require the
current reset's id/title to equal the selected durable transcript rather than a
caller-authored placeholder. In Tasks 2-4, migrate these assertions to the
boxed snapshot shape and assert its optional recorded id and title equal the
runtime handle/saved transcript while history/composer behavior remains
unchanged.

- [x] **Step 5: Run RED commands and record expected failures**

```bash
cargo test -p orca-tui surface_session_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui hosted_tui_rename_updates_durable_title_and_projection_event --lib --locked -- --test-threads=1
cargo test -p orca-tui hosted_tui_fork_preserves_source_and_projects_copied_history --lib --locked -- --test-threads=1
cargo test -p orca-tui hosted_tui_new_session_preserves_old_history_and_starts_with_empty_context --lib --locked -- --test-threads=1
```

Expected: the ephemeral test fails because the current projection invents a
session id; cursor tests fail because identity has no owner; hosted tests fail
because granular or placeholder-bearing identity events are still emitted.
Compilation/setup errors are not accepted RED evidence.

RED evidence on the pre-slice implementation:

- both `surface_session_projection` tests compiled and failed because an
  ephemeral thread received a session id and sequence 1 overwrote sequence 2;
- hosted rename compiled and failed when `SessionRenamed` preceded any
  post-commit projection;
- hosted `/new` compiled and failed because `NewSessionStarted` arrived without
  a preceding reset;
- saved fork and resume compiled and failed because their reset titles were
  `Forked conversation` and `Restored conversation` rather than the runtime
  snapshot titles.

### Task 2: Add The Optional Cursor-Fenced Session Owner

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs:25-230`
- Modify: `crates/orca-tui/src/surface_projection.rs:383-420`
- Modify: `crates/orca-tui/src/types.rs:820-1060`
- Modify: `crates/orca-tui/src/types.rs:1280-1435`

- [x] **Step 1: Correct the projection identity type**

Change `SurfaceProjectionState.session_id` to `Option<String>` and derive it
from persistence:

```rust
let session_id = matches!(
    snapshot.thread.persistence,
    ThreadPersistence::RecordedCatalogued
)
.then(|| surface_thread_id_text(&snapshot.thread.thread_id));
```

Both ephemeral persistence variants must project `None`; title remains the
snapshot title.

- [x] **Step 2: Add session presentation and apply outcome types**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionProjectionPresentation {
    Renamed,
    Forked,
}

pub(crate) enum SurfaceSessionProjectionApply {
    Rejected,
    Accepted(Option<SurfaceSessionProjectionEffect>),
}
```

Extend `SurfaceProjectionState` with
`session_presentation: Option<SessionProjectionPresentation>` defaulting to
`None`, plus a consuming `with_session_presentation` helper.

- [x] **Step 3: Implement `SurfaceSessionProjectionState`**

The private owner contains optional recorded id, optional title, accepted
cursor, and presented cursor. Its transition accepts the first projection,
rejects different thread/incarnation without reset, rejects lower sequence,
accepts higher sequence, accepts equal replay only when id/title agree, and
returns one presentation effect per accepted cursor. `Renamed` and `Forked`
require `session_id.is_some()`.

Expose immutable owner queries through AppState:

```rust
pub fn current_session_id(&self) -> Option<&str>;
pub fn current_session_title(&self) -> Option<&str>;
```

- [x] **Step 4: Replace public mutable AppState fields**

Replace `pub current_session_id` and `pub current_session_title` with
`pub(crate) surface_session: SurfaceSessionProjectionState`. Add a test-only
replacement helper only for isolated renderer/session-picker fixtures that do
not exercise projection behavior.

- [x] **Step 5: Make identity gate the whole snapshot**

At the start of `apply_surface_projection_state`, apply the session owner. On
`Rejected`, return before metrics, Goal, workflow, or operation projection.
On `Accepted`, apply the remaining owners and then render any session effect.
Remove implicit session-change detection by string id.

- [x] **Step 6: Run owner tests to GREEN**

```bash
cargo test -p orca-tui surface_session_projection --lib --locked -- --test-threads=1
```

Expected: ephemeral identity, stale/equal/different-thread fencing, explicit
reset, and once-per-cursor presentation tests pass.

### Task 3: Make Reset And Presentation Snapshot-Only

**Files:**
- Modify: `crates/orca-tui/src/types.rs:235-370`
- Modify: `crates/orca-tui/src/types.rs:1400-1435`
- Modify: `crates/orca-tui/src/types.rs:1750-1800`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs:70-115`

- [x] **Step 1: Change lifecycle event shapes**

Replace the identity-bearing variants with:

```rust
NewSessionStarted,
SessionProjectionReset(Box<SurfaceProjectionState>),
```

Delete `SessionIdentityUpdated`, `SessionRenamed`, and `SessionForked`.
`NewSessionStarted` is control-only and never changes AppState identity.

- [x] **Step 2: Apply reset snapshots atomically**

Change `reset_session_projection` to take no identity strings. Validate reset
presentation before clearing; a rejected envelope must preserve the previous
session owner and transient state. Accepted resets clear the session owner and
all existing session-scoped transient state, then apply the boxed snapshot.
Session presentation effects use the owner-provided authoritative title and
retain the existing acknowledgement wording/status transitions.

- [x] **Step 3: Preserve new-session composer behavior**

Update `handle_runtime_event` and the app event loop to recognize the
payload-free `NewSessionStarted`. It still clears the composer and changes the
local config history mode to `Record`; the preceding reset snapshot supplies
identity.

- [x] **Step 4: Run reset and runtime-event tests**

```bash
cargo test -p orca-tui new_session_started --lib --locked -- --test-threads=1
cargo test -p orca-tui session_projection_reset --lib --locked -- --test-threads=1
```

Expected: session-scoped state resets, authoritative identity is applied, and
composer/runtime settings behavior is preserved.

### Task 4: Preflight Session Switches And Project Rename/Fork

**Files:**
- Modify: `crates/orca-tui/src/surface_client.rs:405-475`
- Modify: `crates/orca-tui/src/surface_actions.rs:140-175`
- Modify: `crates/orca-tui/src/app.rs:7860-8335`
- Modify: `crates/orca-tui/src/hosted_side.rs:60-105`
- Modify: `crates/orca-tui/src/app.rs:8860-9260`

- [x] **Step 1: Preserve metadata commit cursor output**

The in-process runtime API returns `MutationReply<()>`, so preserve its actual
session-family `ThreadLocalCursor` acknowledgement in a crate-private
`TuiSessionMetadataCommit`. Compute the exact next metadata revision from the
validated precondition for compensation, and return both values without
expanding the public runtime API. Existing stale-update tests continue to prove
the revision precondition behavior.

- [x] **Step 2: Return an authoritative rename projection**

After both metadata and saved-session rename commits, read a fresh snapshot.
Require a committed thread cursor, identical thread/incarnation, and
`snapshot.cursor.next_seq >= committed.next_seq`. Convert it to
`SurfaceProjectionState`, attach `Renamed`, and return it. Projection failure
must say `Session rename committed but TUI projection failed` and must not send
the requested title as a fallback.

- [x] **Step 3: Preflight newly started session projections**

Before `install_hosted_session`, read a snapshot from each newly started new,
resume, or fork handle and build its projection. Validate:

```rust
projection.session_id.as_deref() == started.session_id()
```

On failure, shut down/reap the uninstalled handle and leave the old controller
thread/config/preload state untouched. Return the projection alongside the
existing mode/id/title outputs.

- [x] **Step 4: Publish reset snapshots in lifecycle order**

For new, current fork, resume saved, and fork saved:

1. install the preflighted thread;
2. rotate attachment routing;
3. send `SessionProjectionReset(Box::new(projection))`;
4. announce runtime readiness;
5. emit history where applicable;
6. send payload-free `NewSessionStarted` for new only.

Modify `project_hosted_thread` to read and send an authoritative reset snapshot
instead of accepting a caller-authored title. Side return/close activates the
target attachment first, sends reset plus inherited history through the ordered
root path, and releases deferred parent interactions only after that barrier.
Side startup and Side toggles preflight the target projection batch before
installing or activating it; failure leaves the old runtime selected.
Parent-to-Side activation rotates the Side sender/attachment generation before
activation so delayed events from the retired relay cannot mutate the hydrated
Side transcript. Interactions deferred by that retired generation retain their
source attachment and are discarded during retirement, so they cannot replay
as parent approvals or input requests on a later toggle.

- [x] **Step 5: Attach fork presentation to final history projection**

Give `emit_typed_history_snapshot` an optional session presentation parameter.
Current/saved fork passes `Some(Forked)`; startup/resume passes `None`. The
function sends history first and one final snapshot with that directive.

- [x] **Step 6: Make runtime-ready identity projection observable**

`announce_runtime_ready` keeps `MentionRuntimeReady`, reads one snapshot, sends
one ordinary `SurfaceProjectionSynced`, and derives recovery from the same
snapshot. On read failure send a visible error instead of silently omitting
identity and recovery state.

- [x] **Step 7: Run hosted lifecycle tests to GREEN**

```bash
cargo test -p orca-tui hosted_tui_new_session --lib --locked -- --test-threads=1
cargo test -p orca-tui hosted_tui_fork --lib --locked -- --test-threads=1
cargo test -p orca-tui picker_fork --lib --locked -- --test-threads=1
cargo test -p orca-tui hosted_tui_rename --lib --locked -- --test-threads=1
cargo test -p orca-tui resumed_uuid_session --lib --locked -- --test-threads=1
cargo test -p orca-tui hosted_side_switches_project --lib --locked -- --test-threads=1
```

Expected: lifecycle ordering, durable ids/titles, history, compensation, and
presentation behavior pass without granular identity events.

### Task 5: Migrate Readers And Delete Identity Shadows

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/session_picker_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify if compiler evidence requires: other `crates/orca-tui/src/*.rs`

- [x] **Step 1: Migrate production readers**

Replace direct field reads with `current_session_id()` and
`current_session_title()` in status, picker actions, and rendering. Bind the
borrowed values once when ownership/borrowing requires it.

- [x] **Step 2: Delete the app-loop shadow**

Remove `active_session_id` and its assignments from `MentionRuntimeReady` and
new-session handling. Build `TuiExit.session_id` from
`state.current_session_id().map(ToOwned::to_owned)` plus the existing
`HistoryMode` fallback.

- [x] **Step 3: Migrate tests without reopening mutation**

Projection behavior tests construct snapshots. Isolated layout/picker fixtures
may use the `#[cfg(test)]` replacement helper. Existing lifecycle tests inspect
reset/projection payloads and visible messages rather than obsolete events.

- [x] **Step 4: Run identity consumer suites**

```bash
cargo test -p orca-tui session_ --lib --locked -- --test-threads=1
cargo test -p orca-tui status --lib --locked -- --test-threads=1
cargo test -p orca-tui exit_session_id --lib --locked -- --test-threads=1
cargo test -p orca-tui attachment --lib --locked -- --test-threads=1
```

- [x] **Step 5: Verify deletion and ownership boundaries**

```bash
rg -n 'SessionIdentityUpdated|SessionRenamed|SessionForked' crates/orca-tui/src
rg -n '\.current_session_id\b|\.current_session_title\b|current_session_(id|title)\s*=' crates/orca-tui/src --glob '*.rs'
rg -n 'active_session_id|surface_session' crates/orca-tui/src --glob '*.rs'
```

Expected: obsolete variants and app-loop shadow are absent; production reads
are immutable methods; `surface_session` identifies one AppState owner plus its
implementation/tests.

### Task 6: Refresh Architecture Evidence

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-session-identity-owner.md`
- Modify: `docs/superpowers/plans/2026-08-15-tui-session-identity-owner.md`
- Modify if validation reports factual anchor drift: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify if reviewed artifacts change: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`

- [x] **Step 1: Update the roadmap**

Record optional recorded identity, cursor fencing, explicit reset snapshots,
rename/fork presentation, pre-install proof, immutable queries, and old-event/
field/shadow deletion. Keep workflow and operation projection explicitly open.
Do not claim release completion.

- [x] **Step 2: Run the runtime-surface validator**

```bash
node scripts/validate-runtime-surface-contract.mjs
```

Update only validator-reported current-source anchors or reviewed digests. If a
reviewed artifact changes, recompute its exact SHA-256 with `shasum -a 256` and
rerun the validator.

Evidence: `node scripts/validate-runtime-surface-contract.mjs` passed after
refreshing the current manifest event expectations, its SHA-256 digest, and the
validator's harmless same-name baseline for the new routing helper.

- [x] **Step 3: Record exact evidence after fresh gates**

Spec and this plan record the RED failure reasons, focused/full counts, validator
results, reset atomicity, Side ordering, and retired-relay interaction fixes.
Independent-review disposition is recorded after the final re-audit. Operational
rebase, integration, and cleanup evidence belongs in the final handoff after
Task 8. The broad goal and release remain active.

### Task 7: Run Full Verification And Independent Review

**Files:**
- Review every changed file from `git diff --stat` and `git diff`

- [x] **Step 1: Run locked compile and full TUI gates**

```bash
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

Fresh evidence: `cargo check -p orca-tui --tests --locked` passed;
`cargo test -p orca-tui --lib --locked -- --test-threads=1` passed with 1,059
tests; `cargo test --test tui_pty_contract --locked -- --test-threads=1`
passed with 6 tests. This slice does not alter provider lowering or DeepSeek
API behavior, so credentialed real-API smoke is not required. The hosted
lifecycle fixtures and root PTY contract are the relevant real boundaries.

- [x] **Step 2: Run validator and hygiene gates**

```bash
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

Fresh evidence: both Node validator self-tests passed (1/1 each), the direct
runtime-surface validator passed after the manifest/digest and baseline refresh,
`cargo fmt --all -- --check` passed, and `git diff --check` passed.

- [x] **Step 3: Audit every acceptance item and full diff**

Audited all identity producers/readers, cursor comparisons, persistence mapping,
reset atomicity and ordering, deferred Side-parent replay, retired-Side
interaction discard, pre-install failure cleanup, rename post-commit proof,
presentation dedupe, public migration, attachment generation behavior, restart
coverage, manifest/digest anchors, and the full diff. No unrelated worktree file
changed.

- [x] **Step 4: Request independent review**

Use `superpowers:requesting-code-review` with the complete diff and Spec.
Require severity-ordered file/line findings and explicit checks for fabricated
resumability, stale/equal/cross-thread handling, reset ordering, runtime/UI
switch divergence, rename commit ambiguity, duplicate presentation, exit hint,
public compatibility, and missing tests. Resolve every Critical and Important
finding and repeat review until none remain.

Independent re-review found no Critical or Important findings. The final
retired-Side interaction race is covered by
`rotating_side_discards_interactions_queued_by_retired_attachment`; it proves an
approval queued by the old Side relay cannot replay after the next Side-to-parent
switch. Focused attachment-routing and Side identity tests, all full gates, and
diff/hygiene checks passed after that fix.

### Task 8: Commit, Rebase, Integrate, And Clean Up

**Files:**
- Stage only files listed by explicit status/diff inspection

- [ ] **Step 1: Create one semantic feature commit**

After fresh gates and review are clean:

```bash
git commit -m "refactor(tui): own session identity projection"
```

Keep source, tests, Spec, plan, and roadmap in the same independently
revertible commit. Do not include unrelated changes.

- [ ] **Step 2: Rebase latest local main and reverify**

Fetch `origin main`, require a clean main checkout, rebase the feature branch
onto current local `main`, and rerun focused identity tests, locked check, full
serial TUI tests, root PTY, validators, formatting, and diff/status checks.

- [ ] **Step 3: Fast-forward clean local main and verify integrated state**

Fast-forward local `main` to the reviewed commit and rerun full TUI, root PTY,
validators, formatting, and clean status. Do not push, tag, release, or publish
this architecture-only slice.

- [ ] **Step 4: Remove only the owned worktree and branch**

Require the owned worktree to be clean and its HEAD to equal integrated main.
Remove `.worktrees/tui-session-identity-owner`, prune registrations, and delete
`codex/tui-session-identity-owner`. Preserve every unrelated worktree, branch,
and untracked user file.

## Plan Self-Review

- Every Spec behavior maps to a task: competing-path RED evidence (Task 1),
  optional identity and cursor gate (Task 2), reset/presentation event boundary
  (Task 3), producer/preflight migration (Task 4), reader/shadow deletion
  (Task 5), docs (Task 6), verification/review (Task 7), and linear integration
  plus cleanup (Task 8).
- Type names and signatures are consistent: projection session id is optional,
  reset carries a boxed projection, and rename/fork presentation is attached to
  an accepted snapshot.
- Normal, cancellation ownership, failure, retry, disconnect, restart,
  ephemeral, recorded, stale, duplicate, contradictory, and cross-thread cases
  have an owner rule or behavior gate.
- The plan adds no compatibility identity cache, fallback event, worker, retry
  loop, protocol change, or persisted format. Workflow and operation projection
  remain explicit later slices.
- There are no unresolved markers or unspecified deletion conditions.
