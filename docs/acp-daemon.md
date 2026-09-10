# Local ACP Sessions

The opt-in local daemon owns one `RuntimeHost` and the shared session actors.
Editor bridges, the hosted TUI, and headless clients use the same ACP session ID.
The existing standalone TUI and `orca --mode=acp` are unchanged, including on
Windows. Local socket commands currently return an explicit unsupported error
on non-Unix platforms.

## Run and Attach

Start a foreground daemon with the workspace and provider configuration it
should own. It reads credentials in this process only:

```sh
orca --cwd /absolute/workspace daemon
```

The default socket is `$ORCA_HOME/acp/daemon.sock`, or
`~/.orca/acp/daemon.sock` when `ORCA_HOME` is unset. An explicit `--socket`
must name an absolute path in a private directory owned by the current user.
For example:

```sh
orca --cwd /absolute/workspace daemon --socket /private/user-dir/orca.sock
orca attach new --cwd /absolute/workspace --socket /private/user-dir/orca.sock
orca attach SESSION_UUID --cwd /absolute/workspace --socket /private/user-dir/orca.sock
orca attach SESSION_UUID --cwd /absolute/workspace --socket /private/user-dir/orca.sock --exec "inspect the tests"
```

The TUI reports the new session ID. Headless attachment prints that ID on stderr,
replays existing assistant history, then streams the requested turn on stdout.
Headless permission requests are denied, not automatically approved.

Configure an ACP editor to execute:

```sh
orca acp-bridge --socket /private/user-dir/orca.sock
```

The bridge copies newline-delimited standard ACP JSON-RPC between stdio and the
socket. It does not interpret prompts or own a runtime. Clients initialize
normally, then use `session/new`, `session/load`, `session/list`, `session/prompt`
and `session/cancel`. `session/load` requires an exact canonical session UUID
and the daemon workspace. Each connection can attach at most four sessions.
The daemon caps active connections and loaded sessions at 64 each.

## Ownership and Recovery

- A session has one mutation lease. A competing prompt or settings mutation is
  rejected while another turn owns it. Observers keep receiving live updates.
- Model, reasoning, and approval-mode changes use ACP `session/set_model`,
  `session/set_mode`, or `session/set_config_option`, not a private RPC. Mode
  changes cannot exceed the daemon's initial approval policy.
- Each connection retains its own initialized capabilities, runtime connection
  identity, and reverse-request routes. Permission, file, and terminal requests
  go only to the submitting owner; an observer cannot answer them or cancel the
  owner's prompt.
- Losing a client transport does not stop ordinary model work or delete the
  session. Client-owned interactions and resources cannot be transferred by
  matching a JSON-RPC ID on another connection. They fail closed on disconnect.
- A new connection initializes and loads the same ID from the authoritative
  runtime projection. Snapshot replay and subsequent updates share a cursor
  boundary. Prompt responses wait for the owner's observer to flush the terminal
  batch, so completion cannot race ahead of the visible response.
- Reconnection never resends a prompt. An uncertain response is not evidence
  that the prompt was rejected.
- Daemon restart reopens the persisted session projection. In-flight generations
  are interrupted, not silently rerun. Side-effectful tools are not replayed
  merely because an editor reconnects or the daemon restarts.

One daemon serves one canonical workspace and one base configuration. Clients
cannot inject MCP servers, additional directories, path policy, or domain
permissions at attachment. Cold load validates the saved workspace and policy;
incompatible policy requires explicit operator intervention, not automatic
widening. Persisted per-domain session grants are conservatively refused by this
initial implementation.

## Endpoint Lifecycle

The daemon requires a `0700` endpoint directory and creates a `0600` Unix socket.
Accepted peers and connected servers must have the current effective UID.
A retained no-follow lock file is protected by the platform
`ExclusiveFileLock`; it is never unlinked on shutdown. Stale socket removal
requires the singleton lock, socket type/owner/mode validation, a refused
connection, and matching inode identity. Regular files, symlinks, live listeners,
and sockets in public directories are not deleted.

Send SIGINT or SIGTERM to the foreground daemon for graceful shutdown. Runtime
shutdown commits terminal state before client transport teardown. The daemon
removes only its own socket inode. A crash releases the OS lock and can leave a
stale socket for the next start to validate. The lock PID is diagnostic only:
the implementation does not signal processes based on stale PID-file content.

## Optional Projection Metadata

Plain ACP clients do not need an Orca extension. They receive ordinary standard
session updates and snapshot replay. A client maintaining an in-place transcript
replica may explicitly negotiate this optional v1 extension in `initialize`:

```json
{
  "protocolVersion": 1,
  "clientCapabilities": {
    "_meta": { "orca.dev/projection": { "version": 1 } }
  }
}
```

Only opted-in clients receive `session/update.params._meta["orca.dev/projection"]`:

- `version: 1`, `itemId`, `offset`: stable display item identity and UTF-8 byte
  offset. Apply overlapping chunks idempotently; never append snapshot text to
  an old transcript. Inline image items have their own IDs.
- `phase: "reset"`: replace the local transcript replica; snapshot chunks follow.
- `phase: "ready"`: snapshot delivery is flushed; `active` reflects current work.
- `phase: "active"` or `"terminal"`: observer turn state. A terminal may include
  standard `stopReason` or `error`; Orca projection clients also receive the
  typed `terminal` value so failure class, budget, cancellation, and shutdown
  reasons survive transport.
- `phase: "reload_required"`: abandon the replica and reconnect/load.
- `usage`: cumulative surface input/output/cache token counts and
  `estimated_cost_usd_micros`. Standard SDK `usage_update` still carries context
  used/size and cost.

These fields augment ACP content and state notifications; they never replace
ACP requests, capability negotiation, content blocks, or permission responses.
Unnegotiated clients receive no projection metadata. As with standard ACP
loading, an unextended client must clear its local transcript before loading.

## Current Limits

- No Windows named-pipe implementation or automatic daemon spawning.
- Cold restart does not continue an interrupted model request. Some saved
  permission configurations are deliberately rejected rather than reconciled.
- Inline base64 image replay is supported; persisted URL/file-backed image
  presentation still requires further coverage.
- ACP has no standard observer terminal notification. The optional extension
  supplies that state to the hosted TUI; plain observers still receive content.
- The hosted TUI supports text/inline-image prompts, scoped permission choices,
  interrupt, and model selection (including the picker's reasoning option) via
  standard ACP requests. Plan checklists and usage/context metrics are read-only
  projections. Inline prompts and replay previews are bounded to 5 MiB and 600
  images; unsupported images are reported, never fetched from URLs or files.
  Bound mentions restore the unsent draft instead of silently dropping bindings.
  Permission commitment is acknowledged only after a successful prompt response;
  a lost transport leaves delivery unconfirmed. Reconnect retries are bounded to
  five with backoff, replace the transcript, and never resend prompts.
  Workflow, queue, goal, approval-mode/plan mutation, child/side session, history
  mutation, and context-management actions remain explicitly unsupported.
- Arbitrary terminal/client resource state is not migrated between owners.

## Verification

`tests/acp_daemon.rs` drives the production daemon and separate bridge processes
with isolated temporary homes. It covers plain-client sharing, live observation,
single-prompt admission, disconnect/reload/restart, ordering, headless attachment,
endpoint permissions, singleton and stale socket handling, scope rejection,
owner-only permission routing, local-disk model reads despite spoofed observer
resource responses, and no implicit restart execution. Shared-connection unit
coverage in `crates/orca-runtime/src/acp/supervisor.rs` uses the existing
`ReadTextFileExecutor` and real duplex ACP frames to exercise runtime-owned
`fs/read_text_file` calls: only the submitting connection receives the request,
an observer's matching response ID cannot settle it, the owner's response
completes it, and cancellation/disconnect fail closed even with late responses.

Run under the coordinator's serialized build schedule:

```sh
CARGO_INCREMENTAL=0 cargo test --test acp_daemon -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib acp::supervisor::tests::shared_connections_scope_read_text_file_responses_and_fail_closed -- --exact --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --test acp_agent
node scripts/validate-windows-platform-boundaries.mjs
```

Mock-provider process tests are deterministic protocol/runtime evidence, not a
substitute for real DeepSeek streaming or an interactive TUI smoke. Both have
passed; their evidence and the remaining repository release-gate limitations
are recorded in the [capability validation report](reports/2026-09-09-deepseek-capabilities-validation.md).
