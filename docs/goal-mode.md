# Persistent Goal Mode

Orca's TUI supports persistent goals through `/goal`. A Goal is a recoverable,
runtime-owned operation attached to a recorded conversation session. The TUI
submits commands and renders semantic events; it does not schedule continuation
turns or decide whether a model claim is terminal.

## Commands

```text
/goal                         # show the current Goal
/goal <objective>             # create and start a Goal
/goal edit <objective>        # revise the objective and reactivate it
/goal pause                   # pause automatic work
/goal resume                  # start a fresh Goal run
/goal clear                   # delete this session's Goal
```

Persistent goals require recorded history so the runtime has a stable session
identity. `orca exec` does not expose Goal tools and is not a headless Goal
contract.

## Runtime Ownership

One `GoalActor` owns the Goal state machine, SQLite transactions, terminal
audit, usage accounting, and outer-turn ledger. `RuntimeHost` owns the composite
`GoalRun`, generation cancellation, and continuation admission.

Runtime ownership is protected by one shared lease per process and an exclusive
`flock` across processes. Only the first live owner may recover stale runs.
Opening `GoalStore` for reads, projections, or tests never performs recovery as
a side effect.

One Goal outer turn is one admitted provider/tool loop from start to terminal
result. Inner model responses and tool calls never count as Goal turns and
never advance the no-progress threshold.

The turn-end order is:

1. persist the outer turn as in flight
2. run the provider/tool loop with an explicit `GoalTurnContext`
3. accept model terminal intents and return a typed deferred acknowledgement
4. close the outer-turn ledger and record provider usage once
5. run terminal verification when an intent is pending
6. commit one state transition
7. admit or reject the next continuation from `RuntimeHost`

No model tool, TUI callback, thread-local value, or frontend counter can bypass
this order.

## Persistence And Recovery

SQLite is authoritative:

- `$ORCA_HOME/goals.sqlite3` when `ORCA_HOME` is set
- `~/.orca/goals.sqlite3` otherwise

The database uses WAL mode, foreign keys, a busy timeout, schema versioning,
transactional transitions, and idempotent usage-event ids. It records goals,
runs, outer turns, accepted terminal intents, usage events, and transition
history. Rejected intents remain visible in the semantic event journal but do
not create pending rows in `goal_intents`.

On first open, an existing `goals_1.json` is validated and migrated in one
transaction. The source is renamed to a timestamped `goals_1.migrated-*` backup
only after the transaction commits. Malformed data, duplicate session ids, or a
failed rename leave the JSON source untouched and fail closed.

If a process exits with an outer turn in flight, the next Runtime owner holding
the lease changes the Goal to `Paused(Recovery)`, closes the stale run, records
`goal.recovered`, and does not call the provider. `/goal resume` starts a fresh
run id and generation fence; it does not reuse the crashed or paused run.

An active `/goal pause` persists `Paused(User)` before cancellation becomes
visible to the generation. The command returns only after the generation joins,
usage is charged, the outer-turn ledger is closed, and the run is no longer in
flight. Ordinary interrupt and shutdown use the same persist-before-cancel
ordering, so a user stop cannot be rewritten as an infrastructure failure.

## States

| Runtime state | Meaning |
| --- | --- |
| `active` | Eligible for continuation admission |
| `paused(user)` | Yielded to an explicit user action, plan mode, or pending interaction |
| `paused(no_progress)` | Three consecutive outer turns repeat the same model-fixable gap, or eight consecutive outer turns hit the inner-turn ceiling |
| `paused(infrastructure)` | Provider, verifier, persistence, or control-plane failure |
| `paused(waiting_for_workflow)` | A workflow owns the next action |
| `paused(recovery)` | A stale in-flight run was recovered after restart |
| `blocked` | A verifier accepted a non-model-fixable external blocker |
| `budget_limited` | Charged provider plus verifier usage reached the Goal token budget |
| `complete` | A verifier accepted the completion evidence |

The compatibility `ThreadGoal` projection may render `Paused(NoProgress)` as
`stalled`. `usage_limited` remains readable only for migrated historical data.

Only verifier output can create `complete` or `blocked`. A failed provider or
control-plane turn becomes a resumable infrastructure pause; it is not counted
as successful progress and cannot auto-continue.

## Model Protocol

Goal turns expose `get_goal`, `create_goal`, and `update_goal`. Outside a live
Goal capability they are not advertised, and direct calls fail instead of
creating hidden state.

The advertised terminal-intent schema is:

```json
{
  "status": "complete",
  "reason": "focused and workspace tests passed",
  "evidence": [
    {
      "kind": "test",
      "summary": "cargo test --workspace passed",
      "target": "cargo test --workspace"
    }
  ],
  "blocker": null
}
```

For `blocked`, `blocker` must contain one of `user_decision`,
`missing_authority`, `external_state`, `environment_contradiction`, or
`unverifiable_requirement`, plus a summary. A blocked claim that still has
viable model-fixable alternatives is classified as `NotAchieved`, not as a user
blocker.

`update_goal` does not persist a terminal state. It returns one of these typed
acknowledgements:

- `DeferredToTurnEnd`: the claim was recorded and will be audited
- `AlreadyPending`: the intent id was already accepted
- `Rejected`: the model can correct invalid arguments or missing evidence
- `BlockedAgainstInactive`: the Goal is no longer eligible for the claim

Malformed model arguments are recoverable tool results. Missing runtime
capability, a stale generation fence, a closed actor, or persistence failure is
a control-plane failure and stops the outer turn.

## Terminal Verification

Deterministic preflight runs before any verifier request. It rejects stale ids,
active workflows for completion, missing evidence, missing tool terminals,
in-flight state, exhausted verifier budget, and invalid blocker kinds.

DeepSeek verification uses a closed JSON schema, no tools, bounded input and
output, and cancellation propagation. Its usage is charged exactly once with a
`verifier:<outer-turn-id>:<attempt>` event and is also recorded on the outer-turn
ledger. Provider or parse errors produce `Paused(Infrastructure)` rather than an
unbounded retry.

## Continuation Admission

Every outer turn is classified before continuation:

- `Advanced`: a successful turn
- `Interrupted`: a typed budget stop with a committed checkpoint
  (`OperationTerminal::Stopped { resumable: true }`); work may resume
- `Blocked`: a non-resumable budget stop, failure, cancellation, approval, or
  verification failure

The continuation gate rejects `Blocked` and admits `Advanced` or `Interrupted`
only when all of these are also true:

- the Goal is `active`
- no cancellation or shutdown is pending
- no user steer input is queued
- no approval, permission, user-input, or MCP interaction is pending
- no workflow owns the next action
- the operation is not in plan mode
- the completed generation fence has not already admitted a continuation
- Goal and verifier budgets remain available

Every decision emits either `goal.continuation.admitted` or
`goal.continuation.rejected` with the Goal/run/outer-turn ids, current state,
reason code, and continuation counter. A late user steer is persisted into the
transcript before the Goal pauses, so rejecting automatic continuation does not
discard user input.

The cross-turn watchdog is separate from that per-turn gate. A completed
side-effecting tool call or a changed structured task plan is substantive
progress and creates a durable `NULL` barrier in gap history. Model replies and
read-only exploration remain observable activity but do not clear the gap
streak. Three consecutive eligible turns with the same normalized model-fixable
gap pause as `NoProgress`, including resumable budget-stop continuations. An
independent cap pauses after eight consecutive budget-stop outer turns even
when intermediate progress is reported. Token deltas and continuation counts
remain accounting and observability data, not proof of progress.

Before hard walls, pinned system reminders are injected as remaining inner
turns, cost budget, or Goal tokens cross configured thresholds. They ask the
model to finish the current atomic step, update the task plan, and record key
findings. A continued outer turn receives a typed envelope containing the full
objective, continuation trigger, previous status and end reason, budget
snapshot, open gap fingerprint, current task-plan snapshot, and a bounded
previous-assistant checkpoint, with an explicit instruction to verify current
state and resume instead of restarting exploration.

## Internal Context

Goal, plan, runtime, and skill steering are typed `InternalContextFragment`
values stored separately from transcript messages. Each fragment has an id,
kind, origin, content, and token limit. Replacement is by id, so repeated Goal
updates do not grow history.

The DeepSeek adapter renders bounded fragments as a dedicated synthetic system
message after canonical instructions and before conversation history. It never
appends Goal text to a user message or tool result, and it preserves assistant
tool-call/tool-result adjacency. Inactive Goals remove their fragment.

## Observability

The session semantic journal records:

```text
goal.created
goal.run.started
goal.turn.started
goal.intent.requested
goal.intent.acknowledged
goal.turn.finished
goal.verification.completed
goal.transitioned
goal.continuation.admitted
goal.continuation.rejected
goal.paused
goal.recovered
goal.completed
```

The same events flow through the live observer used by TUI and ACP. Frontends
project these records; they do not reconstruct a second Goal state machine.

The real DeepSeek release harness runs completion, rejected completion, genuine
blocked, cancellation, and resume scenarios with an isolated `ORCA_HOME`. Each
scenario compares the live event stream, persisted session JSONL, and SQLite
audit snapshot. It fails on request/ack mismatch, missing verifier usage,
in-flight runs, or any automatic continuation started after a terminal
transition.

## Implementation Map

- Domain types and compatibility projection: `orca-core/src/goal_runtime.rs`,
  `orca-core/src/goal_types.rs`
- State machine: `orca-runtime/src/goal_tracker.rs`
- SQLite and migration: `orca-runtime/src/goal_store.rs`
- Runtime ownership: `orca-runtime/src/goal_actor.rs`,
  `orca-runtime/src/runtime_host.rs`
- Terminal audit: `orca-runtime/src/goal_verifier.rs`
- Pure model protocol: `orca-tools/src/update_goal.rs`
- Role-safe provider context: `orca-core/src/conversation.rs`,
  `orca-provider/src/context.rs`, `orca-provider/src/deepseek_http.rs`
- TUI and ACP projections: `orca-tui/src/runtime_event_projection.rs`,
  `orca-runtime/src/acp/event_map.rs`

The v0.2.46 thread-local control-plane incident is documented in
[`docs/reports/2026-07-18-goal-runtime-control-plane-incident.md`](reports/2026-07-18-goal-runtime-control-plane-incident.md).
