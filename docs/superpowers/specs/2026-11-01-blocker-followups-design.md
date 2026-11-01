# Blocker follow-ups: subagent blocked status, exec question, interactive presentation

Date: 2026-11-01 · Builds on: `2026-11-01-exec-blocker-declaration-design.md` (#156) and
`2026-11-01-superagent-learnings-approach.md` (items C, D, E) · Status: decisions taken in chat
2026-11-01; spec for review.

## Goal

#156 lets a headless `orca exec` root run declare a blocker. It exits 5 with `status: "blocked"`.
Three gaps remain:

- **C.** A subagent that ends blocked is reported to its parent as an ordinary failure, so the
  parent cannot tell a blocker from a crash.
- **D.** A blocked exec run cannot ask the one question that would unblock it. DeepSeek's
  dsh-superagent prototype carries that question as `human_action`.
- **E.** The TUI and ACP render a Blocked terminal poorly. ACP turns it into an internal error, and
  the TUI's next step does not say how to continue.

## Decisions (taken 2026-11-01)

| Item | Decision |
|---|---|
| C | A new `blocked` status for subagent tasks and results, carrying the blocker's `kind` and `summary`. |
| D | `report_blocker` gains an optional `question`. The run still ends `blocked`, exit 5. The answer comes back through `orca exec resume <session> "<answer>"`. No new status or exit code. |
| E | Presentation only. Interactive sessions do not get `report_blocker`. The TUI and ACP learn to present a Blocked terminal that already reaches them. |

## C. A blocked subagent reports `blocked` to its parent

### Which subagents can be blocked

Today `report_blocker` is offered only to a headless root (`AgentToolPolicyContext::headless`,
depth 0). Under C a child agent gets the tool too, when its parent run is headless exec:

- A headless root's subagents (depth ≥ 1) are offered `report_blocker`.
- A custom agent gets `report_blocker` only when its definition lists it, because its frozen tool list is authoritative.
- Interactive, ACP and server roots, and their subagents, still are not. A child of an interactive
  root reports its blocker in prose, as today.
- Goal mode still does not get it, at any depth.
- A child's `report_blocker` ends only the child. The parent's run continues.

### What the parent sees

| Layer | Today | After C |
|---|---|---|
| `RuntimeTaskStatus` (`runtime_lifecycle.rs:31`) | `Failed` | new `Blocked` |
| `TaskStatus` (`orca-core/src/task_types.rs:9`) and `SurfaceTaskStatus` (`projection.rs:1146`) | `Failed` | new `Blocked`; terminal, not active, no attention |
| `SurfaceSubagentStatus` / `SurfaceSubagentTerminalStatus` (`projection.rs:1414`, `:1531`) | `Failed` | new `Blocked` |
| The parent's `subagent` tool result | `Subagent status: Failed …` | `Subagent status: Blocked`, then `kind: <kind>`, `summary: <summary>`, and `question: <question>` when D's field is set |

The tool result for a blocked child is a failed tool result, `ToolStatus::Failed`. The child did
not deliver what was asked, and existing code treats any non-success child as failed. Only the
text and the task status carry `Blocked`. Mapping sites to change:
- `runtime_subagent_call.rs:1493` and `:2278`;
- `subagent_async_worker.rs:654`;
- `child_agent_types.rs:702` (child tool activity keeps `Failed`, because activity status
  describes a tool call, not the child).

### Persistence

`TaskStatus`, `SurfaceTaskStatus` and the subagent statuses are serialized into the task registry,
history and the surface store.

- **Reading.** Old records never contain `blocked`, so reading them is unchanged.
- **Downgrade.** A newer record that contains `blocked`, read by an older binary, fails to
  deserialize that record. Acceptable for a 0.x release; the release notes must say it.
- **Schema versions.** No schema-version bump unless a store that pins one rejects the variant;
  the implementer checks and reports.

### Consumers that must handle the new status

- The TUI task panel and the agents dock render `blocked` as its own label, with the summary.
- ACP and the server adapter map a blocked subagent task to their existing failed or
  ended-with-error shape. Neither has a blocked notion, and this spec adds none.
- `task_list` and `task_status` output show `blocked`.
- Workflow agents are out of scope and keep `Failed`, unless the shared task registry forces the
  change.

## D. An optional question on `report_blocker`

### Schema

`report_blocker` gains:

- `question`: string, optional, at most 1,000 characters. One concrete question whose answer
  would let the run continue.

The tool description says to set it only when a person's answer would unblock the work, and to
ask exactly one question.

### Where it appears

- `BlockerSummary` (`orca-core/src/goal_runtime.rs:123`) gains `#[serde(default,
  skip_serializing_if = "Option::is_none")] question: Option<String>`. Goal mode never sets it, so
  goal records are unchanged.
- `session.completed.terminal.blocked.blocker.question` carries it, through the existing
  serialization of the terminal.
- In text mode, a blocked exec run prints the question on its own line, followed by the resume
  hint, when the run recorded history:

  ```text
  Blocked: <summary>
  Question: <question>
  To continue this session, run: orca exec resume <session-id> "<your answer>"
  ```

  The existing resume hint (`controller.rs:1734`) already fires for non-success runs. Only
  the question line and the quoted-answer form are new.

### Answering

The answer is the prompt of the resumed run: `orca exec resume <session-id> "<answer>"`. Nothing
else is needed:

- The resumed run appends to the transcript.
- The model sees its own `report_blocker` call and the person's answer, and continues.

`docs/harness-contract.md` documents this flow in the Run Status section. In JSONL mode the
`session_id` is present only with `--save-history`. The contract says that resuming a blocked run
needs it.

### Validation

`parse_report_blocker_args` (`orca-tools/src/report_blocker.rs`) enforces the 1,000-character
cap, and also rejects an empty or whitespace-only `question` instead of storing it.

## E. Interactive presentation of a Blocked terminal

No new tool in TUI, ACP or server sessions. The changes:

- **TUI** (`orca-tui/src/diagnostics.rs:323`). The Blocked diagnostic stays `operation.blocked`,
  titled "Task blocked", with the summary as its detail.
  - **Next step when a question is present:** "Answer the question below, then continue." The
    question is rendered on its own line.
  - **Next step without a question:** "Resolve what is missing, then continue."

  The current text "submit the request again" is replaced, because a resumed session continues
  where it stopped.
- **ACP** (`acp/agent.rs`, `terminal_to_stop_reason`). A Blocked terminal returns
  `Ok(StopReason::EndTurn)` instead of `Err("blocked")`. The blocker summary, and the question when
  present, are sent to the client as the turn's final agent message before the stop, so an IDE
  shows why the turn ended. Today the error surfaces as a JSON-RPC internal error.
  - An interactive root never produces Blocked under this spec. A Blocked terminal reaches ACP
    only from a resumed headless session or a blocked subagent's parent turn, so this path is rare.
  - The test pins the mapping.
- **Server mode** already maps Blocked to `"blocked"` (`server/surface_adapter.rs:2652`). No
  change.

## What stays the same

- Exit code 5 and `status: "blocked"` stay as #156 defined them. D adds a field, not a status.
- `report_blocker` stays unavailable to interactive, ACP, server and goal-mode roots.
- A child's blocker never ends its parent's run.
- `--verifier` still does not upgrade a blocked run.

## Tests

**C:**
- A headless root whose subagent calls `report_blocker` gets a tool result containing
  `Subagent status: Blocked` with the blocker kind and summary. The task registry records `Blocked`.
  The root run continues and can still succeed.
- An interactive (`unrestricted`) root's subagent is not offered the tool.
- Serde round-trips of `TaskStatus::Blocked`, `SurfaceTaskStatus::Blocked` and the subagent
  statuses.

**D:**
- `parse_report_blocker_args` accepts a question, rejects one over 1,000 characters, and rejects an
  empty one.
- An exec e2e test: `session.completed.terminal.blocked.blocker.question` carries the question.
- A text-mode run prints the question line and the resume hint.
- The goal-mode `BlockerSummary` serialization is unchanged without a question. Pin an existing goal
  fixture.

**E:**
- A TUI diagnostic test for a Blocked terminal with and without a question (next-step text).
- An ACP test: a Blocked terminal maps to `EndTurn`, and the final message carries the summary.

## Documentation

- `docs/harness-contract.md`:
  - the `question` field;
  - the resume-to-answer flow and its `--save-history` requirement;
  - the subagent `blocked` status in the tool and task sections.
- `site/src/docs/md/{en,zh}/orca-exec.mdx`: one paragraph on answering a blocked run's question
  with `orca exec resume`.
- Release notes (v0.5.15):
  - exit 5;
  - `blocked`;
  - the question field;
  - the subagent status;
  - `writes_applied`;
  - the downgrade caveat for records that contain `blocked`.
