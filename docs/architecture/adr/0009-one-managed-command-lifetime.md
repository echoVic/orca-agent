# ADR 0009: One Managed Command Lifetime

- Status: Accepted
- Date: 2026-09-13
- Supersedes: ADR 0006 (model-facing tool surface and default lifetime only;
  the `TerminalService` design it introduced is retained)
- Scope: model-facing command execution, waiting, output reading, stopping

## Context

ADR 0006 added `TerminalService` and exposed it as two model tools,
`exec_command` and `write_stdin`. That left Orca with two parallel command
paths and two inconsistent lifetimes:

- `bash` was the familiar name but ran its own temporary session manager,
  forced a pipe, closed stdin, and blocked until the process exited or hit a
  hard-coded 120-second timeout. A long build, a CI watch, or an interactive
  program could not survive a tool call.
- `exec_command` used the retained terminal service but was a second,
  overlapping start entry point with a different result shape, so the model had
  to choose between two tools with different lifecycles and had to translate
  between `session_id` and `task_id` for control operations.

The 120-second default was also doing two jobs at once. It capped how long a
tool call waited and how long the process could live, so a slow but healthy
command was killed by a policy the caller never chose and could not see.

## Decision

`bash` is the only model-visible command entry point, and it uses the retained
terminal service. `exec_command` and `write_stdin` are removed with no
compatibility aliases. `bash`'s previous independent execution path is deleted.

**The lifetime of a command belongs to the task, not to the tool call.** A
tool call starts, observes, or controls a command. Returning from a call,
ending a model reply, paging output, or losing a client connection never
implies killing the process.

The model surface is one starter plus three non-overlapping operations:

| Tool | Meaning | Can change execution state |
|---|---|---|
| `bash` | start one command; wait at most `yield_time_ms`, then return | yes |
| `task_read_output` | read produced output at a caller-owned cursor | no |
| `task_send_input` | write to a `pty` task's stdin, optionally closing it | yes |
| `task_wait` | wait for a state change or terminal on one or more tasks | no |
| `task_stop` | request a stop of the task and its managed tree | yes |
| `task_list` | list accessible tasks | no |

Every one of these addresses work by `task_id`. `session_id` is an internal
identity and is no longer part of the model contract.

Two independent time concepts replace the single timeout:

- `yield_time_ms` (default 1000, 0–30000) bounds only how long the call waits.
  Its expiry returns `running` and leaves the process untouched.
- `timeout_ms` (default none, positive only) is an execution deadline measured
  from process start. It is the only caller-side limit on process lifetime.
  0 is rejected because it would mean both "expire now" and "no limit".

The effective deadline is the earliest of the caller's `timeout_ms`, any
existing task deadline, and the administrator's `shell_timeout_secs` cap, and
the response names the source. `shell_timeout_secs` keeps its meaning as an
explicit administrator policy but its default becomes 0 (no cap), so no command
is silently bounded by a value the caller never set.

`bash` accepts `terminal: "pipe" | "pty"`, `workdir`, and
`lifetime: "task" | "workspace"`. `lifetime` changes ownership, never
permission. There is deliberately no `run_in_background` flag: every command
can outlive its call, so a flag would imply otherwise.

### Result contract

Every command result separates why the call returned from why the process
ended:

- `return_reason`: `yield_elapsed`, `terminal_observed`, or
  `deadline_exceeded`.
- `termination_reason`: null while running, otherwise the observed termination.

A command that is still running reports a new `running` tool status. It is not
`completed` and not a success: there is no exit code, and the caller keeps
observing it. A terminal observation is never reopened to `running` by late
output. An unknown or expired `task_id` is an explicit error, never an empty
success.

## Consequences

- `ToolStatus`/`ToolResultKind` gain `Running`. It maps to a returned tool call
  in the surface projection, so a yield renders like any other returned call
  while the authoritative state stays on the task record. Turn status and
  surface status do not treat a yield as a failure.
- Model-visible output is bounded per call (`max_output_tokens`, default 2000)
  and the retained archive is bounded per task; a bounded read never stops
  draining the pipe.
- Cursor reads are per caller. Two readers asking for the same offset get the
  same bytes; a cursor never advances a shared position. A finished session now
  stays addressable for the retention window instead of being discarded at
  EOF, so a completion notification's output can actually be paged.
- Deadlines terminate the managed process tree, and the terminal is recorded as
  `timed_out` with `deadline_reached` set. A deadline-killed command never
  looks like a clean exit.
- Process startup and approval keep their existing gates. `bash` keeps
  `ActionKind::Shell` and `CapabilitySet::shell_execute()`, so the approval path
  is unchanged; the sandbox continues to come from
  `prepare_shell_command`, which already applied the permission profile and
  turn overlay. Retiring the old path removed a second sandbox derivation that
  hard-coded `network_access: true`.
- `ToolCapability::TerminalTransport` and the pre-side-effect permission helper
  it needed are removed; `bash` never had a mid-call permission escalation.
- ADR 0006 remains the record of the `TerminalService` supervisor design
  (single-owner supervisor thread, bounded mailbox, 25 ms maintenance interval,
  retained output archive). Only its model-facing tool list and default
  lifetime are superseded here.

## Verification

Deterministic tests cover:

- exactly one model-visible tool can start a process, and the removed tools are
  gone from the registry;
- the `bash` schema separates `yield_time_ms` from `timeout_ms` and has no
  background flag;
- a running command is reported as `running` with no exit code and
  `return_reason: yield_elapsed`;
- a finished command reports `terminal_observed`, its exit code, and its
  termination;
- a deadline stop reports `deadline_exceeded`, names the deadline source, and
  never reports exit code 0;
- a deadline actually terminates the managed process tree and the task record
  does not claim completion;
- a yield expiring does not terminate the command;
- cursor reads are idempotent per caller, reject an out-of-range cursor, and
  report an unknown task explicitly;
- a terminal observation is not reopened to `running` by later reads;
- `timeout_ms: 0` is rejected as ambiguous.
