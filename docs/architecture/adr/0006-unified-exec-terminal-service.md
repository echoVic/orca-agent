# ADR 0006: Unified Exec Terminal Service

- Status: Accepted; released in v0.3.23
- Date: 2026-08-17
- Updated: 2026-09-09 (durable output and explicit offset polling)
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
  `chars` is omitted. Optional `output_offset` reads one archived page without
  consuming the automatic cursor.

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

## Durable Output

Recorded task registries own an `output/` directory inside their existing
`tasks/<session>/` storage directory. The SQLite archive stores a shell-session
to task mapping, terminal mode, terminal outcome, automatic cursor, absolute
byte counts, per-stream prefix counts, and ordered UTF-8 output chunks. It is
primary output storage, not a derived index that can be silently rebuilt.
Process-local registries retain their existing memory-only lifecycle.

Shell readers commit each observed chunk before publishing it to the bounded
memory tail. SQLite uses FULL synchronization and DELETE journaling. Terminal
outcomes are committed only after both output readers have joined. This covers
`exec_command`, `bash`, and server shell/command adapters; the latter keep their
existing bounded snapshot wire behavior. A persistence failure is latched and
reported as an error rather than a successful empty output. This guarantee
starts at the reader's committed chunk, not at bytes still in an OS pipe.

Limits are 32 MiB retained per task, 128 MiB per session, 256 task metadata
rows, and 32,768 chunk rows. Appends and reads operate on at most 8 KiB chunks
(plus up to three UTF-8 boundary bytes). Per-task trimming rounds the retained
start up to a UTF-8 boundary; session pressure removes oldest chunks. Older
completed task entries expire when new tasks need metadata capacity. A full
set of active tasks rejects a new launch. Absolute offsets never reset.
Expired task entries return an explicit missing/expired error.

The memory tail is bounded to 8 MiB and 32,768 chunks per store, SQLite's cache
to 2 MiB, and each archive read to 256 KiB plus UTF-8 rounding. The database
page ceiling includes metadata overhead (about 274 MiB at default limits);
DELETE journals are bounded by the database's pages and FULL auto-vacuum
reclaims deleted pages. The terminal service retains at most 256 completed
session objects for ten minutes; freeing an object or observing EOF only
evicts memory, not the archive. Deleting the owning task-session directory
also deletes its archive. There is no separate global archive directory.

Roots must be absolute and session IDs must be unambiguous single components.
Reads accept a shell ID, never a filename or arbitrary task path. The archive
verifies its recorded session ID and rejects linked roots, linked database or
journal files, and replaced files. Unix output directories/files require
current-user ownership and private 0700/0600 permissions; hard-linked files
are rejected. Windows rejects reparse points and inherits the task store ACL.
The fixed macOS `/tmp` and `/var` system aliases are resolved before SQLite's
no-follow open. Explicit reads use the registered `write_stdin` transport
through the existing tool authorization path.

A process-exclusive file lock prevents a second host from taking over an
active archive. Managers within one host share that owner. After the owner
exits, a new instance reopens the mapping without scanning all output into
memory; any remaining `running` archive metadata becomes `interrupted`, with
no exit code. Completed status and the automatic cursor survive reopening.
This does not claim that OS processes or stdin survive a runtime host restart.

## Result Contract

Both tools return JSON containing:

- `session_id` and `task_id`;
- process `status`, `termination`, and `exit_code`;
- bounded incremental output;
- output cursor and truncation metadata;
- requested and effective terminal modes.

Offset polling example:

```json
{"session_id":"shell-<id>","output_offset":0,"max_output_tokens":2000}
```

Use `next_output_offset` for the next page. Offsets address the combined
stdout/stderr stream in reader-observed order, measured in bytes of normalized
UTF-8 text (invalid process bytes become replacement characters).
`output_offset` echoes the request; `output_bytes_total` is the current total.
The start and page end round up inside a UTF-8 code point, matching the existing
reader contract. A page may exceed its byte budget by at most three bytes.

Repeated offsets return the same bytes while those bytes remain retained,
independently of other reads. Live totals/status may change and retention may
advance the available prefix. `omitted_prefix_bytes` counts unavailable bytes
between the requested offset and retained start. `truncated` means omitted
prefix or additional available bytes. `eof` means a terminal process outcome
and a cursor at the current total; an empty live poll is not EOF. Offset equal
to the total is valid; future, negative, fractional, and overflowing offsets
are invalid. Missing, corrupt, or inaccessible archives produce errors.

Explicit-offset reads are immediate (`yield_time_ms` is ignored), do not
acknowledge background notifications, and cannot be combined with nonempty
`chars`. Omitting the offset preserves automatic-cursor polling, stdin writes,
and completion acknowledgement.

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
- model-visible registry and target normalization;
- initial/next/repeated explicit pages, EOF and invalid cursors;
- UTF-8 boundaries, task/session byte caps and metadata row caps;
- completed and interrupted restart recovery in a new host process;
- missing archives, symlinks, private modes and cross-session isolation;
- automatic-cursor independence and rejection before stdin side effects;
- a real 9 MiB process capture, bounded paging, and reopen throughput.
