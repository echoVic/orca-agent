# Subagent Observability, User Trust, And Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Orca one durable, identity-bound path from child execution through runtime projection to an expandable TUI, exact permissions, completion proof, recoverable session discovery, and truthful onboarding/diagnostics.

**Architecture:** Child loops emit one typed ordered activity envelope. Synchronous execution commits it through a thread-actor ingress; detached workers append it to a fenced relay that the actor drains through the same ingress. The runtime surface remains the only TUI fact source. Permission decisions, transcript reads, completion proof, and stored-session health are typed runtime boundaries rather than adapter or filesystem behavior. Old compatibility paths are deleted.

**Tech Stack:** Rust, Tokio, serde, length-prefixed/checksummed local records, SQLite derived index, ratatui/crossterm, Clap, Cargo tests, PTY contract tests, Node documentation validators, Astro site build.

---

## File Map

- `crates/orca-runtime/src/child_agent_types.rs`: fallible activity sink and
  typed child events.
- `crates/orca-runtime/src/child_agent_loop_runner.rs`: complete turn/tool/usage
  activity emission.
- `crates/orca-runtime/src/runtime_subagent_call.rs`: synchronous sink wiring.
- `crates/orca-runtime/src/subagent_async_worker.rs`: detached relay writer.
- `crates/orca-runtime/src/subagent_event_relay.rs`: durable ordered relay.
- `crates/orca-runtime/src/tasks.rs`: lease-fenced relay admission and latest
  task mirror.
- `crates/orca-runtime/src/runtime_surface/{projection,reducer,ingress}.rs`:
  authoritative subagent types, invariants, and ingress.
- `crates/orca-runtime/src/runtime_host.rs` and
  `crates/orca-runtime/src/runtime_actor/*`: atomic publication, relay draining,
  restart recovery, transcript reads, permission transaction, and completion
  proof.
- `crates/orca-tui/src/{surface_projection,workflow_panel,protocol}.rs`: joined
  tree projection, local expansion, typed permission response.
- `crates/orca-tui/src/{idle_key_actions,queued_input_actions,ui}.rs`: tree and
  transcript interaction/rendering in idle and running states.
- `crates/orca-runtime/src/thread_store/{writer,session_index,local,types}.rs`:
  bounded scanner, logical quarantine, search isolation, and health DTOs.
- `crates/orca-runtime/src/diagnostics.rs`, `src/cli.rs`, and TUI setup modules:
  doctor and first-run contract.
- `site/src/docs/md/{en,zh}/*.mdx`, root READMEs, and contract scripts: canonical
  package/domain/CLI/config documentation.

### Task 1: Freeze Typed Child Activity And Surface Invariants

**Files:**
- Modify: `crates/orca-runtime/src/child_agent_types.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/projection.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/reducer.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/identity.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_types.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_reducer.rs`

- [ ] **Step 1: Add RED type and reducer tests**

Require task/subagent/attempt identity, structured phase/tool activity, parent
task preservation, exact source-sequence successors, duplicate idempotence,
conflicting digest rejection, stale attempt rejection, detached-task authority,
and terminal absorption. Require start and terminal fixtures to update task and
subagent in the same batch.

- [ ] **Step 2: Observe the RED failures**

```bash
cargo test -p orca-runtime --test runtime_surface_types --locked
cargo test -p orca-runtime --test runtime_surface_reducer --locked -- --test-threads=1
```

Expected: fail because existing patches carry only display activity and a
generation parent.

- [ ] **Step 3: Implement the minimal current schema**

Add `SubagentActivityEvent`, payload/phase/current-tool/owner types, task parent
and checkpoint/transcript bindings, and source cursor fields. Make reducer
validation enforce exact owner, attempt, sequence, digest, and atomic task join.
Version the current schema and reject old records; do not add conversion shims.

- [ ] **Step 4: Run focused GREEN tests and format**

```bash
cargo test -p orca-runtime --test runtime_surface_types --locked
cargo test -p orca-runtime --test runtime_surface_reducer --locked -- --test-threads=1
cargo fmt --all -- --check
```

### Task 2: Add The Fenced Durable Relay

**Files:**
- Create: `crates/orca-runtime/src/subagent_event_relay.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/tasks.rs`
- Test: `crates/orca-runtime/tests/subagent_event_relay.rs`

- [ ] **Step 1: Add RED relay contract tests**

Cover reopen and ordered paging, same-digest idempotence, conflicting digest,
sequence gap, stale lease, old attempt, concurrent append serialization,
partial final record, corrupt middle record, bounded record/file size, and
append-before-latest-snapshot crash repair. Assert source task artifacts are not
rewritten on quarantine.

- [ ] **Step 2: Observe the RED test target does not exist**

```bash
cargo test -p orca-runtime --test subagent_event_relay --locked -- --test-threads=1
```

- [ ] **Step 3: Implement length-prefixed checksummed records**

Store relay records below the task-session store. Use the existing task lock and
validate task type, owner, lease epoch, attempt, contiguous sequence, digest, and
size on every append. Return `Appended` or `AlreadyApplied`; return typed conflict,
gap, stale-owner, corruption, and limit errors. Update the repairable latest task
mirror only after the append succeeds.

- [ ] **Step 4: Run relay and task tests**

```bash
cargo test -p orca-runtime --test subagent_event_relay --locked -- --test-threads=1
cargo test -p orca-runtime tasks --lib --locked -- --test-threads=1
```

### Task 3: Wire Complete Child Activity Through One Ingress

**Files:**
- Modify: `crates/orca-runtime/src/child_agent_types.rs`
- Modify: `crates/orca-runtime/src/child_agent_loop_runner.rs`
- Modify: `crates/orca-runtime/src/child_agent_provider_turn.rs`
- Modify: `crates/orca-runtime/src/runtime_subagent_call.rs`
- Modify: `crates/orca-runtime/src/subagent_execution.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/ingress.rs`
- Modify: `crates/orca-runtime/src/runtime_actor/thread_actor_generation.rs`
- Modify: `crates/orca-runtime/src/tool_execution.rs`
- Modify: `crates/orca-runtime/src/tool_router.rs`
- Test: `crates/orca-runtime/src/child_agent_tests.rs`
- Test: `tests/subagent_contract.rs`

- [ ] **Step 1: Add RED observer and sync integration tests**

Require started before any progress, turn/phase, structured tool start/completion,
usage, checkpoint, and exactly one terminal event. Assert every event uses the
same task/subagent/attempt identity and contiguous source sequence. Add a sink
failure test proving execution stops and an active unsafe tool becomes
indeterminate.

- [ ] **Step 2: Observe RED**

```bash
cargo test -p orca-runtime child_agent --lib --locked -- --test-threads=1
cargo test --test subagent_contract --locked -- --test-threads=1
```

- [ ] **Step 3: Replace the infallible observer**

Introduce `ChildAgentActivitySink::publish -> io::Result<()>`, preserve the
source streaming throttle, and emit complete loop boundaries. Add
`RuntimeSubagentActivityIngress` using the workflow lifecycle ingress as the
ownership template. Thread it from hosted generation through tool dispatch into
the synchronous child wrapper. Remove the production child `io::sink()` event
discard path.

- [ ] **Step 4: Commit sync activity atomically**

Have the actor assign one stable commit ID per source event and commit joined
task/subagent patches. Project legacy headless events only after the surface
commit. Do not make transient streaming chunks semantic session-journal records.

- [ ] **Step 5: Run GREEN and obsolete-path search**

```bash
cargo test -p orca-runtime child_agent --lib --locked -- --test-threads=1
cargo test --test subagent_contract --locked -- --test-threads=1
rg -n 'EventSink::new\(io::sink\(\).*subagent|subagent_progress\(' crates/orca-runtime crates/orca-core
```

Production sync paths must use the typed sink; `subagent_progress` must have a
real projection bridge caller.

### Task 4: Drain Detached Activity Across Restart And Reconnect

**Files:**
- Modify: `crates/orca-runtime/src/subagent_async_worker.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_actor/mod.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/ingress.rs`
- Modify: `crates/orca-runtime/src/tasks.rs`
- Test: `tests/subagent_contract.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_commit.rs`
- Test: `crates/orca-runtime/tests/runtime_host.rs`

- [ ] **Step 1: Replace the async no-start assertion with RED recovery tests**

Assert async launch returns promptly but a committed started event becomes
visible without `subagent_status`. Kill/recreate the host while the worker keeps
running; require replay of every missing event once, continued live delivery,
and terminal convergence. Add stable-commit retry, slow-subscriber gap/snapshot,
stale takeover, and terminal-relay-not-drained cases.

- [ ] **Step 2: Observe RED**

```bash
cargo test --test subagent_contract --locked -- --test-threads=1
cargo test -p orca-runtime runtime_host --lib --locked -- --test-threads=1
```

- [ ] **Step 3: Inject the relay sink into the worker**

Write started/progress/checkpoint/terminal records under the active task lease.
Keep continuation-terminal-before-task-terminal ordering. A relay write failure
must produce a typed indeterminate or failed task outcome; it must not be
silently swallowed.

- [ ] **Step 4: Add the actor-owned drainer**

On attach/start and task publication wakeups, drain after the surface cursor for
active and not-yet-drained terminal tasks. Reuse each event's stable commit ID,
probe the surface ledger after ambiguous commit failures, and deduplicate by
attempt/sequence/digest. Bound each page and reschedule until caught up. Quarantine
one bad relay without blocking other tasks.

- [ ] **Step 5: Run GREEN and restart stress tests**

```bash
cargo test --test subagent_contract --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_host --lib --locked -- --test-threads=1
```

### Task 5: Add Task Tree And Checkpoint-Backed Child Transcript

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/commands.rs`
- Modify: `crates/orca-tui/src/surface_projection.rs`
- Modify: `crates/orca-tui/src/workflow_panel.rs`
- Modify: `crates/orca-tui/src/protocol.rs`
- Modify: `crates/orca-tui/src/idle_key_actions.rs`
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: `crates/orca-tui/src/app_integration_tests.rs`
- Test: `crates/orca-tui/src/surface_boundary_tests.rs`

- [ ] **Step 1: Add RED runtime transcript tests**

Require a typed transcript snapshot from a valid child checkpoint and typed
checkpoint-unavailable, stale-task-revision, and cross-task-binding failures.
Assert no filesystem path is returned.

- [ ] **Step 2: Add RED TUI tree tests**

Cover parent-preserving projection, indented flattening, expansion retention,
collapse selection repair, live phase/tool/turn/usage, left/right navigation,
Enter approval precedence, Enter transcript open, stale transcript response,
Esc return order, running-state navigation, narrow viewport, and session reset.

- [ ] **Step 3: Observe RED**

```bash
cargo test -p orca-tui workflow_panel --lib --locked -- --test-threads=1
cargo test -p orca-tui subagent --lib --locked -- --test-threads=1
cargo test -p orca-tui surface_boundary --lib --locked -- --test-threads=1
```

- [ ] **Step 4: Implement the typed transcript query**

Load only the continuation checkpoint bound to the expected task revision,
validate its digest and identity, and project user-visible messages/tool items,
turn, usage, checkpoint revision, and complete flag. Do not expose hidden
reasoning or paths.

- [ ] **Step 5: Implement local tree state and read-only detail state**

Join tasks and subagents in `SurfaceProjectionState`, stop assigning known
fields to `None`, store only local expansion/selection/detail state in the TUI,
and route panel keys before idle/running composer handling. Delete filesystem
transcript presentation and any TaskRegistry fallback.

- [ ] **Step 6: Run GREEN and PTY interaction tests**

```bash
cargo test -p orca-tui workflow_panel --lib --locked -- --test-threads=1
cargo test -p orca-tui subagent --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

### Task 6: Bind Permission Grants To Child Activity Atomically

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/interaction.rs`
- Modify: `crates/orca-runtime/src/runtime_actor/thread_actor_interaction.rs`
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Modify: `crates/orca-runtime/src/acp/agent.rs`
- Modify: `crates/orca-runtime/src/server/processors/permission.rs`
- Modify: `crates/orca-runtime/src/server/surface_adapter.rs`
- Modify: `crates/orca-tui/src/protocol.rs`
- Modify: `crates/orca-tui/src/approval_actions.rs`
- Modify: `crates/orca-tui/src/operation_controller.rs`
- Modify: `crates/orca-tui/src/hosted_session.rs`
- Test: `tests/session_server_contract.rs`
- Test: `crates/orca-tui/src/app_integration_tests.rs`

- [ ] **Step 1: Add RED cross-surface permission matrix**

For TUI, ACP, JSONL, and headless, assert the same exact profile and turn/session
scope, task/subagent/attempt/activity owner, policy epoch, backend/enforcement
provenance, first-response-wins, and stale-owner rejection. Simulate commit
failure and prove no session grant remains. Mark stderr-inferred suggestions as
non-authoritative.

- [ ] **Step 2: Observe RED**

```bash
cargo test -p orca-runtime permission --lib --locked -- --test-threads=1
cargo test --test session_server_contract --locked -- --test-threads=1
cargo test -p orca-tui permission --lib --locked -- --test-threads=1
```

- [ ] **Step 3: Add owner/evidence and one actor transaction**

Bind interactions to operation, turn, tool, optional task/subagent/attempt/source
sequence, profile digest, and policy epoch. Commit session settings grant,
interaction resolution, and task pending-interaction state in one surface batch.
Only wake the waiter after the commit. Plumb capability/sandbox receipts and
safe denial provenance.

- [ ] **Step 4: Migrate every adapter to the typed transaction**

Replace TUI bool answers with explicit decision/scope/profile. Make ACP
`allow_always` a real session grant. Remove JSONL pre-persistence and all
permission use of local always-tool/target allowlists. Preserve structured
commit rejection reasons.

- [ ] **Step 5: Delete obsolete rails and run GREEN**

Delete legacy queue permission handling, adapter-owned persistence, retired
permission compatibility variants, and byte-compatibility fixtures.

```bash
cargo test -p orca-runtime permission --lib --locked -- --test-threads=1
cargo test --test session_server_contract --locked -- --test-threads=1
cargo test -p orca-tui permission --lib --locked -- --test-threads=1
rg -n 'Permission\(bool\)|persist_session_permission_grant' crates
```

The final search must be empty.

### Task 7: Make Completion Proof A Terminal Contract

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/projection.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/reducer.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-tui/src/surface_projection.rs`
- Modify: `crates/orca-tui/src/transcript_state.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_reducer.rs`
- Test: `crates/orca-tui/src/state_integration_tests.rs`

- [ ] **Step 1: Add RED proof transition tests**

Require verified only with relevant successful checks, failed check retention,
explicit unverified completion, bounded output, indeterminate limitation,
terminal immutability, and restart hydration. Test concise and expanded TUI
rendering.

- [ ] **Step 2: Observe RED**

```bash
cargo test -p orca-runtime completion_proof --lib --locked -- --test-threads=1
cargo test -p orca-tui completion_proof --lib --locked -- --test-threads=1
```

- [ ] **Step 3: Commit proof with terminal operation state**

Add `SurfaceCompletionProof` and derive it only from committed verifier results
and evidence. Record limitations for skipped verification, cancellation,
timeout, degraded projection, lost worker, stale checkpoint, and indeterminate
tools. Do not infer verification from exit text or model claims.

- [ ] **Step 4: Project the proof into the transcript**

Render one stable terminal summary and an expandable detail view. Preserve
command, exit status, bounded output summaries, evidence targets, and
limitations across restart.

- [ ] **Step 5: Run GREEN**

```bash
cargo test -p orca-runtime completion_proof --lib --locked -- --test-threads=1
cargo test -p orca-tui completion_proof --lib --locked -- --test-threads=1
```

### Task 8: Isolate Corrupt Sessions And Bound Search

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/types.rs`
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`
- Modify: `crates/orca-runtime/src/thread_store/session_index.rs`
- Modify: `crates/orca-runtime/src/thread_store/local.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-tui/src/session_picker_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: `tests/thread_store_contract.rs`

- [ ] **Step 1: Add RED bounded-reader and quarantine tests**

Cover valid input, valid unterminated final JSON, true EOF truncation, complete
invalid final JSON, malformed middle record, typed semantic errors, oversized
line/decoded bytes/record count, and zstd bomb. Require exact typed health and no
source mutation.

- [ ] **Step 2: Add RED catalog/search/UI tests**

Mix healthy, bad metadata, bad middle, and bad zstd sessions. Require every
entry to remain visible across index rebuild and stable pagination; healthy
search hits survive corrupt files; diagnostics carry health; picker gates
resume/fork/rename but retains reference/archive/delete actions. Startup catalog
failure must be visible.

- [ ] **Step 3: Observe RED**

```bash
cargo test -p orca-runtime thread_store --lib --locked -- --test-threads=1
cargo test --test thread_store_contract --locked -- --test-threads=1
cargo test -p orca-tui session_picker --lib --locked -- --test-threads=1
```

- [ ] **Step 4: Implement one bounded streaming scanner**

Share plaintext/zstd limits and return health, issue code, safe location, and
fingerprint. Only EOF-truncated final records are recoverable. Stop silently
skipping arbitrary malformed records and whole-file collection.

- [ ] **Step 5: Cache derived health and isolate failures**

Persist health/fingerprint in the rebuildable index, retain unreadable metadata
by storage identity, make pagination compensate for isolated rows, and return
per-file search diagnostics without failing the whole request. Add health to
runtime/JSONL/TUI DTOs without overloading live surface health.

- [ ] **Step 6: Run GREEN**

```bash
cargo test -p orca-runtime thread_store --lib --locked -- --test-threads=1
cargo test --test thread_store_contract --locked -- --test-threads=1
cargo test -p orca-tui session_picker --lib --locked -- --test-threads=1
```

### Task 9: Implement Read-Only Doctor And Honest First Run

**Files:**
- Create: `crates/orca-runtime/src/diagnostics.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `crates/orca-core/src/config/file.rs`
- Modify: `crates/orca-tui/src/setup_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `site/src/docs/md/en/*.mdx`
- Modify: `site/src/docs/md/zh/*.mdx`
- Create: `scripts/validate-public-cli-contract.mjs`
- Create: `scripts/test-validate-public-cli-contract.mjs`

- [ ] **Step 1: Add RED doctor and docs contract tests**

Test stable text/JSON, secret redaction, no filesystem mutation, unknown trust,
malformed config, required-failure exit one, warnings exit zero, sandbox backend
states, custom `ORCA_HOME`, and current cwd. Reject old package/domain, fake
commands, and unsupported flags in public docs.

- [ ] **Step 2: Observe RED**

```bash
cargo test -p blade-deepseek cli:: --lib --locked
cargo test -p blade-deepseek --test cli_architecture_contract --locked
node --test scripts/test-validate-public-cli-contract.mjs
```

- [ ] **Step 3: Add the library-owned doctor boundary**

Implement `DiagnosticReport` and offline `collect_doctor`. Reuse canonical
config/home/trust and platform sandbox probes. On Windows call only the helper's
read-only check. Never provision, write, start a server, or invoke a provider.
Expose `doctor --cwd --format text|json` as a real Clap subcommand.

- [ ] **Step 4: Make first run use the same facts**

Show the resolved auth path, cwd, trust, and sandbox readiness. Keep credential
write and trust mutation explicit. Unknown trust remains untrusted. Add setup
transition and custom-home render tests.

- [ ] **Step 5: Rewrite EN/ZH public docs literally**

Use `@blade-ai/orca`, `orcaagent.dev`, actual command/flag output, actual config
precedence and paths, and explicit doctor limitations. Delete rather than alias
`orca config`, `orca sessions`, old package/domain, and unsupported examples.

- [ ] **Step 6: Run doctor and site GREEN gates**

```bash
cargo test -p blade-deepseek cli:: --lib --locked
cargo test -p blade-deepseek --test cli_architecture_contract --locked
cargo test -p orca-tui setup --lib --locked -- --test-threads=1
node --test scripts/test-validate-public-cli-contract.mjs
node scripts/validate-public-cli-contract.mjs
cargo run --quiet -- doctor --help
ORCA_HOME="$(mktemp -d)" cargo run --quiet -- doctor --format json
npm --prefix site run build
```

### Task 10: Cross-Surface Qualification And Deletion Review

**Files:**
- Modify: affected contract validators and focused specs only as required by
  intentional current-schema changes.
- Modify: this spec and plan status/evidence.

- [ ] **Step 1: Run focused cross-surface contracts**

```bash
cargo test --test subagent_contract --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
cargo test --test session_server_contract --locked -- --test-threads=1
cargo test --test thread_store_contract --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
```

- [ ] **Step 2: Run architecture validators and obsolete searches**

```bash
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node --test scripts/test-validate-public-cli-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
node scripts/validate-public-cli-contract.mjs
rg -n '@orcla/cli|orca\.ai|orca config|orca sessions|Permission\(bool\)|persist_session_permission_grant' README* site crates src
```

The final search must be empty except explicit negative fixtures in the public
contract validator.

- [ ] **Step 3: Run complete fresh verification**

```bash
cargo test --workspace --locked -- --test-threads=1
cargo check --workspace --all-targets --locked
node --test scripts/release/test-stage-npm.mjs
node --test scripts/release/test-verify-published.mjs
npm --prefix site run build
cargo fmt --all -- --check
git diff --check
git status --short
```

- [ ] **Step 4: Exercise real PTY crash and recovery scenarios**

Use deterministic test providers/tools to prove child visibility before the
first tool, continuing updates during tools, parent restart replay, tree
navigation while running, exact permission target/scope, child transcript open,
terminal proof, and corrupt-session picker isolation. Capture commands and
results as completion evidence.

- [ ] **Step 5: Independent review and final cleanup**

Review for a second task source, unfenced relay writes, unsafe tool replay,
duplicate projection, private reasoning exposure, orphan grants, fabricated
verification, source transcript mutation, public contract drift, and lingering
compatibility code. Fix findings with RED tests, rerun affected gates, then rerun
fresh full verification before marking the goal complete.

## Plan Self-Review

The order freezes schema before storage, storage before execution wiring,
execution before presentation, and runtime authority before adapter/UI changes.
Each task has an observable RED test, a bounded ownership change, deletion of the
replaced path, and a focused GREEN gate. Restart, duplicate, corruption, stale
identity, permission failure, hidden reasoning, indeterminate side effects,
first-run truth, and full release-facing verification are explicit rather than
left to integration luck.
