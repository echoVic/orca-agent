# ADR 0006: Unified Exec Terminal Service

- Status: Accepted; released in v0.3.23
- Date: 2026-08-17
- Updated: 2026-08-18 (background supervisor and completion notifications)
- Scope: model-facing shell execution, interactive stdin, PTY, task control

## Context

Orca already had a capable `RuntimeShellSessionManager`: process-tree cleanup,
pipe and PTY modes, stdin, resize, incremental output, sandbox integration, and
task registration. The model-facing `bash` tool did not retain that capability.
Each call created a temporary manager, closed stdin immediately, and waited for
completion or timeout. Interactive programs such as editors, REPLs, and TUIs
therefore could not be continued across tool calls, and terminal control
characters such as Ctrl-U had no persistent PTY session to target.

The JSONL server had a separate long-lived command/process adapter over the
same low-level shell manager. Adding another PTY implementation would have
created a third lifecycle and cleanup path.

## Decision

Add a thread-owned `TerminalService` in `orca-runtime` and expose two canonical
model tools:

- `exec_command` starts a command and returns a `session_id` and `task_id`. If
  the command has not completed by `yield_time_ms`, it remains running.
- `write_stdin` writes characters to the retained session or polls it when
  `chars` is omitted.

The service is stored in the runtime thread's typed extension store. Every
turn in the same thread resolves the same service instance. A dedicated
single-owner supervisor thread owns the shell-session manager and all mutable
terminal session state; callers communicate through a bounded mailbox rather
than sharing the manager behind a mutex.

The supervisor wakes on commands and on a 25 ms maintenance interval. It
actively reaps natural process exits and stop requests recorded in the shared
task registry, so process completion, task settlement, network-proxy release,
and process-tree cleanup no longer depend on another `write_stdin` poll. This
also lets TUI and runtime-surface task stop requests use the existing
`TaskRegistry` control path without a second terminal-specific protocol.

When a command outlives the initial `exec_command` yield, the supervisor may
enqueue one bounded completion record. Before the next model turn, Orca drains
these records into pinned system task notifications containing the terminal
status, exit code, and bounded output. A terminal result already observed by
`write_stdin` is acknowledged and removed from the queue, preventing duplicate
notifications. The queue is bounded to 64 records and each notification keeps
at most 8 KiB of output.

Dropping the runtime thread sends an explicit shutdown command, terminates all
remaining process trees, releases retained resources, and joins the supervisor
thread before returning.

`exec_command` uses the same active permission profile, writable roots,
network-domain policy, sandbox mode, task registry, and process-tree ownership
as `bash`. PTY allocation is explicit through `tty`; pipe mode remains the
default for deterministic non-interactive output. The existing `bash` tool is
kept unchanged for compatibility.

`write_stdin` is a transport operation for a command that has already passed
shell approval. It does not request shell approval again, is not classified as
a read-only concurrent tool, and may carry raw terminal control characters.

`task_stop` keeps its existing task-registry behavior and additionally asks the
thread-owned terminal service to terminate the matching process tree
immediately. Unread output is preserved until the terminal result is polled.

## Result Contract

Both tools return JSON containing:

- `session_id` and `task_id`;
- process `status`, `termination`, and `exit_code`;
- bounded incremental output;
- output cursor and truncation metadata;
- requested and effective terminal modes.

A non-zero command exit is represented inside this JSON. The tool transport
itself completes successfully when Orca observed the process result, allowing
the model to inspect and respond to the exit status without treating the
runtime transport as indeterminate.

Background completion notifications are advisory conversation context, not a
new autonomous model turn. They are delivered exactly once at the next normal
turn boundary unless the model already observed the terminal result directly.

## Compatibility

- `bash` retains its synchronous timeout and result semantics.
- Existing task IDs, task listing, hook configuration, sandbox profiles, and
  persisted tool-call records remain valid.
- Hooks configured for `bash` also match `exec_command`; `write_stdin` does not
  inherit shell-command approval.
- Runtime event and thread-store projection recognize `exec_command` as command
  execution.
- JSONL `shell/*` and `command/exec` remain protocol adapters over the existing
  low-level shell-session manager. This change does not alter their wire
  shapes.

## Verification

The focused contract tests cover:

- fast command completion;
- a running pipe session continued through stdin;
- PTY line editing with Ctrl-U;
- process-tree termination through task control;
- natural completion without a follow-up terminal poll;
- task-registry stop settlement without a terminal poll;
- exactly-once completion queueing and poll acknowledgement;
- isolated output across concurrent sessions;
- supervisor shutdown and descendant process cleanup;
- next-turn completion notification injection;
- model-visible registry and target normalization.
