# Headless exec blocker declaration — design

Date: 2026-11-01 · Issue: #156 · Status: approved in chat 2026-10-31, written 2026-11-01

## Goal

A headless `orca exec` run that cannot make progress because of something outside
the workspace must report `blocked` instead of `success`, with a nonzero exit code.
Today such a run ends with `status: "success"` and exit 0, so automation and evals
that trust the exit code miss the failure (issue #156, D-7-honest-blocker).

The model declares the blocker through a new terminal tool, `report_blocker`, so the
runtime never guesses from the model's text. D-7 measures that the model uses it.

## Scope

**In scope:** headless `orca exec` root sessions only.

**Out of scope:** TUI, ACP, server, subagents, and goal mode. Goal mode already has
`update_goal` with a `Blocked` state; it keeps its own mechanism. Subagents are a
separate child loop and a separate result contract.

## The tool: `report_blocker`

A new built-in tool, available only in headless `orca exec` root sessions. It is a
**terminal tool**: calling it ends the turn, the way a final message ends it with
`Success`.

### Arguments

Reuses the goal-mode vocabulary so the two stay consistent:

| field | type | cap |
|---|---|---|
| `kind` | `BlockerKind` — `user_decision`, `missing_authority`, `external_state`, `environment_contradiction`, `unverifiable_requirement` | required |
| `summary` | `String` | required, ≤ 2000 chars (same as `MAX_GOAL_BLOCKER_SUMMARY_CHARS`) |
| `evidence` | `Vec<EvidenceItem>` | optional, ≤ 32 items (same as `MAX_GOAL_EVIDENCE_ITEMS`) |

`BlockerKind` and `EvidenceItem` are the existing types in
`crates/orca-core/src/goal_runtime.rs`. The tool reuses them, not a copy.

### Behavior

A successful `report_blocker` call ends the agent loop with the new `Blocked`
status. The model is free to explain the blocker in the same turn; the tool's
result is returned to the model only to confirm the call was accepted, then the
turn ends.

### Prompting

The exec system prompt and the tool description tell the model: if you cannot
make progress because of something outside your control (a read-only workspace,
missing credentials, no network, a requirement you cannot satisfy), call
`report_blocker` instead of claiming done or stopping. This relies on the model
using it; the D-7 evidence shows the model was already honest in prose
("Blocked — the workspace is mounted read-only").

## Status and exit code

### `RunStatus`

Add `Blocked { blocker: BlockerSummary }` to `RunStatus`
(`crates/orca-core/src/event_schema.rs:333`).

- `as_str()` returns `"blocked"`.
- `exit_code()` returns `5` — the next free code after `0/1/2/3/130`. Budget
  exhaustion already uses `4` in the headless path (`controller.rs:1956`).

### `OperationTerminal`

Add a `Blocked { blocker: BlockerSummary, usage: BudgetUsage }` variant to the
core `OperationTerminal` (`crates/orca-core/src/budget.rs:170`). `usage()` returns
`Some(usage)`, like `Completed`.

`AgentLoopResult::terminal_from_status`
(`crates/orca-runtime/src/lifecycle.rs:150`) maps `RunStatus::Blocked` to
`OperationTerminal::Blocked`. The blocker travels on the `AgentLoopResult`; the
helper reads it from the result that produced the status. This keeps the single
reconstruction site for non-budget exits intact.

### `session.completed`

The JSONL terminal record (`crates/orca-runtime/src/runtime_host.rs:23179-23197`)
emits `status: "blocked"` and the terminal carries the blocker. The payload shape
follows the existing `event_schema.rs:1278-1300` mapping: `status`, `terminal`,
`completion_proof`, optional `session_id`.

### Exit code

The headless exit-code path (`controller.rs:1945-1969`) maps the surface terminal
to a code. Blocked maps to `5`, the way `BudgetExhausted` maps to `4`. The signal
override (128 + signo) still wins.

## Where it plugs in

The choke point is the post-loop block in `PreparedThreadTurn::execute`
(`crates/orca-runtime/src/controller.rs:880-939`), which already holds the loop
result and feeds the JSONL record, the exit code, and the surface terminal. The
`--verifier` override already lives there and changes the status; the Blocked
path follows the same shape.

1. **Agent loop:** `report_blocker` ends the loop with
   `AgentLoopResult { status: Blocked, terminal: OperationTerminal::Blocked { .. },
   .. }`, set at the call site (mirrors how `RuntimeProviderResponseOutcome::Success`
   becomes `AgentLoopResult::success` at `provider_turn.rs:891`).
2. **Post-loop block:** passes `Blocked` through unchanged, the way `Success`
   passes through. `run_verifier_if_needed` runs only when the status is `Success`
   (`controller.rs:902`), so a Blocked run does not run the verifier. Decision:
   blocked is terminal; the verifier is a downgrade gate, not a positive one, so
   it cannot upgrade blocked → success.
3. **JSONL:** the headless record uses `Blocked` → `status: "blocked"`.
4. **Exit code:** the surface terminal maps to `5`.
5. **Surface terminal:** `finish_surface_operation`
   (`thread_actor_operation.rs:5753`) records the `Blocked` terminal. The typed
   surface is changed only enough to carry the new terminal; ACP and server do not
   read it (see below).

## What stays the same

- A turn that ends with a final message, no `report_blocker` call, still reports
  `success` exit 0. This only adds a new way to end a turn.
- Goal mode keeps `update_goal`. Its `Blocked` state is unchanged.
- TUI, ACP, server, and subagents see no new tool and no status change. They are
  not in scope.
- `--verifier` still runs on `Success` and can downgrade to `verification_failed`.
  It does not run on `Blocked`.
- The journal terminal is written once before the post-loop block; the Blocked
  path writes it the same way every other non-budget exit does.

## Clients that read the same terminal

The research found these consumers. Each is unaffected or only needs to stay
consistent with the new status:

- **ACP:** a non-approval `Failed` becomes a JSON-RPC internal error. `Blocked` is
  not `Failed`, and exec is not an ACP client, so ACP is untouched. The new
  terminal variant must not break `terminal_mapping_preserves_only_exact_standard_stop_reasons`
  (`acp/agent.rs:3819`); if that mapping is exhaustive over `OperationTerminal`, add
  `Blocked` to it as a no-op (it is never produced for ACP).
- **Server:** the legacy `turn_completed` copies `session.completed.status`
  (`protocol/events.rs:731`); the typed adapter uses `terminal_status`
  (`server/surface_adapter.rs:2638`). The new status string must be added where the
  server enumerates statuses, if any such enumeration exists.
- **Goal mode:** stops continuing on anything but `Succeeded`. A Blocked turn is
  not a goal turn (goal mode is out of scope), so this is unchanged.
- **Notifications and text mode:** the resume hint keys off the status
  (`controller.rs:1708-1719`). Blocked is not success, so the hint does not fire;
  that is the desired behavior.

## Tests pinning current behavior

The research lists tests that pin exit 0 after a failed tool call and exit codes
for denied edits. None of them call `report_blocker`, so none need to change. The
new tests pin the new path:

- **Unit:** the tool parses args, validates `kind`/`summary`, caps `evidence`.
- **Integration:** a headless run that calls `report_blocker` ends with
  `status: "blocked"`, exit `5`, and the blocker in the terminal record.
- **Contract:** `tests/operation_terminal_contract.rs` and `tests/exec_jsonl.rs`
  pin the new status and exit. If `terminal_mapping_preserves_only_exact_standard_stop_reasons`
  is exhaustive, the new terminal variant is added there.
- **D-7 re-run:** the model is expected to call the tool; the verifier's check
  (`terminal == "success" or exit_code == "0"` → fail) then passes because the
  terminal is `blocked` and the exit is `5`.

## Documentation

- `docs/harness-contract.md` gets the new status (`blocked`) at the status list
  (~186-196) and the new exit code (`5`) at the exit-code table (~236-244), with a
  one-line note that it means the model declared a blocker.
- The site doc `site/src/docs/md/en/orca-exec.mdx:64-66` says Orca publishes no
  stable numeric exit taxonomy. That wording is reconciled: the harness contract
  is the source of truth for the headless exit codes, and the site doc points to
  it.

## Open decisions called

- **Exit code 5** (next free after budget's 4).
- **Verifier does not upgrade blocked → success.** Blocked is terminal.
- **`report_blocker` is on for every exec run by default**, not behind a flag. One
  extra tool schema is cheap; the alternative (opt-in) would mean D-7 has to pass
  a flag the harness does not set today.
- **No text-based claim detection.** The runtime never reads the model's text to
  decide completion. This respects the existing rule that verification truth never
  comes from assistant text (`projection.rs:149-150`).

## Constraints a guard would have to respect

Carried from the research, for the implementer:

- The scope is the outer turn, not the last iteration. `report_blocker` ends the
  outer loop.
- The JSONL status, exit code, transcript/task status, and journal must all agree.
  The journal terminal is written once; the post-loop block is the consistency
  point.
- `OperationTerminal` is persisted with serde. Adding a variant affects stored
  data and any exhaustive match over it (the ACP mapping, the surface reducer, the
  TUI projection). Each must add the variant or remain non-exhaustive.
