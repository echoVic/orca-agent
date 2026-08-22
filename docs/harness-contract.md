# Orca Harness Contract

This document defines the external contract for `orca exec`.

The contract covers: a headless command, a versioned JSONL event stream, approval events, tool events, verification events, and deterministic exit codes.

## Command

```sh
orca exec [options] <prompt>
```

Options:

- `--output-format text|jsonl` — Output format (default: text)
- `--cwd <path>` — Workspace directory
- `--approval-mode suggest|auto-edit|full-auto` — Approval policy (default: auto-edit)
- `--verifier <command>` — Post-completion verification command
- `--model <name>` — Model override
- `--base-url <url>` — API base URL override

## Embedded Server Protocol

```sh
orca --mode=server
```

Server mode reads one JSON object per line from stdin and writes one JSON object per line to
stdout. It retains the legacy one-shot `submit` operation and also exposes stateful thread, turn,
shell, command, permission, file-search, and Mention-search methods. The legacy shape is:

```json
{"id":1,"op":"submit","prompt":"fix the bug in main.rs"}
```

The response stream preserves the request `id` and emits compact protocol events derived from the normal runtime event stream:

```jsonl
{"id":1,"event":"turn_started","turn":1}
{"id":1,"event":"reasoning_delta","text":"Let me look..."}
{"id":1,"event":"tool_requested","tool":"read_file","target":"src/main.rs"}
{"id":1,"event":"tool_completed","tool":"read_file","status":"completed"}
{"id":1,"event":"message_delta","text":"I found the issue..."}
{"id":1,"event":"turn_completed","status":"success"}
```

Unsupported operations and malformed requests emit an `error` event. Server mode exits when stdin closes.

Legacy unbound `submit` runs as a one-shot request. Thread-bound turns and search sessions may keep
running while the server accepts later control/update lines. Events are streamed as they occur and
preserve the request id that owns the stream.

### Streaming file search

The Codex-compatible file-only search protocol accepts multiple explicit roots:

```json
{"id":"files-start","method":"fuzzyFileSearch/sessionStart","params":{"sessionId":"files-1","roots":["/workspace/frontend","/workspace/backend"],"exclude":["target/**"],"respectGitignore":true,"resultLimit":24}}
{"id":"files-update","method":"fuzzyFileSearch/sessionUpdate","params":{"sessionId":"files-1","query":"src/main"}}
{"id":"files-stop","method":"fuzzyFileSearch/sessionStop","params":{"sessionId":"files-1"}}
```

`fuzzyFileSearch/sessionUpdated` notifications stream `files` with canonical `root`, relative
`path`, file/directory `matchType`, score, matched character indices, phase, and scan progress.
Equal relative paths from different roots remain separate results. If roots overlap, the same
filesystem path may appear once per owning root traversal; clients should treat `id`/`root + path`
as identity rather than deduplicating on the relative path alone.

### Unified Mention search

Unified search is bound to a live thread so discovery uses that thread's workspace roots and MCP
registry:

```json
{"id":"thread","method":"thread/start","params":{"runtimeWorkspaceRoots":["/workspace/frontend","/workspace/backend"]}}
{"id":"mentions-start","method":"mention/search/start","params":{"sessionId":"mentions-1","threadId":"thread-id-from-response","resultLimit":12}}
{"id":"mentions-update","method":"mention/search/update","params":{"sessionId":"mentions-1","query":"review"}}
{"id":"mentions-stop","method":"mention/search/stop","params":{"sessionId":"mentions-1"}}
```

`mention/search/updated` notifications merge file, Skill, Plugin, MCP Resource, and MCP Resource
Template candidates. Every candidate includes `id`, `kind`, `display`, `description`, score,
highlight indices, and a typed `target`. The candidate id is opaque and derived from the complete
typed target; clients must preserve it for selection anchors and must not reconstruct it from
display text. Catalog discovery errors are returned in `errors` without discarding healthy
candidates.

### Atomic Mention input

Clients submit the exact selected target instead of sending only display text:

```json
{
  "id": "turn",
  "method": "turn/start",
  "params": {
    "threadId": "thread-id-from-response",
    "input": [
      {"type":"text","text":"compare "},
      {
        "type":"mention",
        "name":"same.txt",
        "target":{
          "type":"file",
          "root":"/workspace/backend",
          "path":"same.txt",
          "kind":"file"
        }
      }
    ]
  }
}
```

Supported targets are `file`, `skill`, `plugin`, `resource`, and `resource_template`. The visible
prompt remains natural (`compare @same.txt`), while the runtime revalidates and expands the bound
target before it enters model history. Bound MCP Resources are read through the same thread
registry used during discovery. Plain text input never infers a Mention: unbound `@...` text remains
literal, even when it names an existing file. Explicit `$skill` prompts remain supported.

## Event Envelope

Every JSONL line is one event:

```json
{
  "version": "1",
  "run_id": "run-...",
  "seq": 0,
  "timestamp_ms": 1780647978857,
  "type": "session.started",
  "payload": {}
}
```

## Event Types

- `session.started`
- `turn.started`
- `assistant.reasoning.delta`
- `assistant.message.delta`
- `provider.replay.updated`
- `approval.requested`
- `approval.resolved`
- `tool.call.requested`
- `tool.call.completed`
- `subagent.started`
- `subagent.completed`
- `verification.started`
- `verification.completed`
- `error`
- `session.completed`

## Run Status

The final `session.completed` event contains one of:

- `success`
- `failed`
- `cancelled`
- `approval_required`
- `verification_failed`
- `budget_exhausted`

When the run recorded history, the same event also carries the durable
`session_id` (the id a subsequent `orca exec resume <session_id>` accepts), so
a harness can continue a budget-exhausted or failed session without parsing the
transcript. In text mode, a non-success exit prints the exact resume command:

```text
To continue this session, run: orca exec resume <session-id>
```

A resumed run appends to the original transcript and owns a fresh budget scope:
the previous invocation's `max_budget` ceiling does not carry over, while its
usage records remain durable.

To restore only the durable message boundary, pass `--resume-at <MESSAGE_ID>`
(a persisted conversation item id) to the `resume` subcommand or to
`--resume`/`--continue`. Messages after the boundary — including uncommitted
tool calls — are not replayed to the model, and an unknown boundary fails
closed before the provider is called:

```text
orca exec resume <session-id> --resume-at <message-id> "continue"
```

When a headless session stops at `budget_exhausted`, the runtime also appends
a typed `session.checkpoint` record to the transcript before the terminal
projection:

```json
{"type":"session.checkpoint","session_id":"...","status":"budget_exhausted",
 "reason":"max_inner_turns","budget_consumed":{"input_tokens":120,...},
 "last_committed_message_id":"item_...","resumable":true,
 "task_plan":"...","recorded_at":"..."}
```

The checkpoint is audit data — execution always resumes from the transcript's
committed messages, and uncommitted side effects are never claimed as
exactly-once (restore repairs them as indeterminate). File rewind is not
promised: Orca does not snapshot external workspace state.

## Exit Codes

- `0`: success
- `1`: failed
- `2`: verification failed
- `3`: approval required or denied
- `4`: budget exhausted
- `130`: cancelled

## Tool Contract

Built-in tools:

| Tool | Action | Description |
|------|--------|-------------|
| `read_file` | read | Reads UTF-8 file content, truncated at 8KB |
| `glob` | read | Finds files and directories by glob pattern or `mode: "fuzzy"` path query, sorted as workspace-relative paths; returns `(no matches)` when the path is missing or no entries match |
| `list_files` | read | Compatibility alias for directory listing; returns sorted names and `(empty)` for missing directories |
| `grep` | read | Regex search with line numbers; uses `rg` when available and a native in-process fallback otherwise, `(no matches)` for empty results |
| `git_status` | read | Runs `git status --short` |
| `web_search` | network | Searches the web for current information |
| `bash` | shell | Executes via `sh -c` under the active approval policy and sandbox |
| `edit` | write | Exact text replacement under the active approval policy and sandbox |
| `write_file` | write | Creates or overwrites a file under the active approval policy and sandbox |
| `subagent` | agent | Runs a synchronous child agent with `description` and `prompt`, returning the child summary |
| `Workflow` | agent | Starts a background dynamic workflow |
| `update_plan` | read | Updates the visible plan state |
| `get_goal` | read | Reads active persistent goal state while goal mode is running |
| `create_goal` | read | Creates a persistent goal while goal mode is running and no unfinished goal exists |
| `update_goal` | read | Submits an evidence-bearing complete/blocked intent for turn-end verification while Goal mode is running |
| `ask_user_question` | read | Asks 1-4 structured questions through the interactive runtime; each question has a header and 2-4 described options, with optional preview and multi-select support |

Tools are registered through a canonical registry. Each tool spec declares its capability set, renderer hint, exposure, aliases, and concurrent-safety flag. Runtime approval derives from the resolved tool spec instead of a separate hard-coded name list. Tool arguments are validated before execution with common JSON Schema object keywords, enums, arrays, and `oneOf` / `anyOf` composition.

`ask_user_question` accepts `questions` with 1-4 entries. Each entry requires a
`header` of at most 12 characters, a non-empty `question`, and 2-4 distinct
`options` containing `label` and `description`; `preview` and `multiSelect` are
optional. Orca asks the questions in order through the runtime-owned interaction
broker. In the TUI, questions with options open a focused choice dialog: arrow
keys move, Enter submits a single choice, and Space toggles multi-select choices.
Typing switches to the composer for a custom answer. A completed call returns
compact JSON as `{"answers":{"question":"answer"}}`.
Dismissal cancels the whole tool call. Headless execution fails deterministically
instead of waiting for input. `ask_user_question` is the only registered and
model-visible user-question tool.

Tool events:
- `tool.call.requested` — emitted before execution, contains `name`, `action`, `target`
- `tool.call.completed` — emitted after execution, contains `name`, `status` (completed/failed/denied), `output`, `truncated`

External tools:
- Orca loads `~/.orca/tools/*.toml` at startup.
- Each descriptor defines `name`, `description`, `action_kind`, `command`, and `schema`.
- Descriptors are advertised to the model as function tools.
- Commands run from the workspace directory with raw JSON arguments always on
  stdin and, up to 64 KiB, mirrored in `ORCA_TOOL_ARGS` for compatibility.

`glob` is the preferred file discovery tool. It accepts the existing `pattern` argument for glob searches and `{"mode":"fuzzy","query":"..."}` for fuzzy path discovery. `list_files` remains accepted for compatibility but is not recommended in the system prompt.

Hook stdout protocol:
- `{"action":"allow"}` allows the operation.
- `{"action":"deny","reason":"..."}` blocks the hook target.
- `{"action":"modify","modified_target":"..."}` rewrites a tool target.
- `{"action":"inject","context":"..."}` injects model context.
- When JSON declares an `action`, unsupported actions and malformed action
  payloads fail the hook instead of being silently injected or ignored.
- Non-JSON stdout and JSON without `action` are treated as injected context.

Subagent events:
- `subagent.started` — emitted when the child agent starts, contains `id`, `description`
- `subagent.completed` — emitted when the child agent finishes, contains `id`, `description`, `status`, `output`, `error`

Persistent goal mode:
- `/goal` is a TUI feature, not a headless `orca exec` contract.
- Goals are keyed by saved TUI session id and stored transactionally in `$ORCA_HOME/goals.sqlite3` or `~/.orca/goals.sqlite3`. A validated `goals_1.json` is migrated once and backed up only after commit.
- One runtime `GoalActor` owns state, outer-turn accounting, terminal intents, usage, and recovery. `RuntimeHost` owns the composite GoalRun and continuation admission; the TUI does not run a continuation loop.
- Runtime owners share one in-process lease and use an exclusive cross-process `flock`. Only the first owner recovers stale in-flight runs; opening a `GoalStore` reader has no recovery side effect.
- `get_goal`, `create_goal`, and `update_goal` are advertised only while a live Goal turn carries an explicit `GoalTurnContext`. Outside Goal mode they return failed tool results instead of creating hidden state.
- `update_goal` accepts only evidence-bearing `complete` or typed `blocked` intents. The acknowledgement is deferred; only turn-end verifier output can persist `complete` or `blocked`.
- Rejected terminal intents emit request/ack events but do not enter the SQLite pending-intent ledger. Accepted/deferred acknowledgements and persisted intent rows must match exactly.
- Progress and stop policy use closed outer turns and structured verifier gaps. Inner model/tool iterations and token deltas do not advance the no-progress threshold.
- Continuation is rejected for queued user input, cancellation, pending interaction, active workflow ownership, plan mode, duplicate generation fences, inactive state, or exhausted budget.
- Pause, verifier/control-plane failure, crash recovery, and no-progress are typed resumable reasons. Resume starts a fresh run and generation fence; recovery never calls the provider automatically.
- Active pause, interrupt, and shutdown persist `Paused(User)` before cancellation. `/goal pause` returns only after generation join, usage settlement, outer-turn closure, and clearing the in-flight run.
- Goal/plan/runtime/skill steering uses bounded `InternalContextFragment` system messages outside transcript history. Tool-result content is never modified.
- `goal.intent.requested`, `goal.intent.acknowledged`, turn/verification/transition events, typed continuation admitted/rejected events, and `goal.paused`, `goal.recovered`, and `goal.completed` are journaled and sent to TUI/ACP observers.

## Approval Policy

Four modes control which tool actions require user confirmation. `auto-edit`
is autonomous within the workspace sandbox; crossing that boundary still uses
the runtime permission flow. `full-auto` removes both the approval prompt and
the default sandbox boundary.

| Mode | read | write | network | agent | shell |
|------|------|-------|---------|-------|-------|
| `suggest` | allow | ask | ask | ask | ask |
| `auto-edit` (default) | allow | allow | allow | allow | allow |
| `full-auto` | allow | allow | allow | allow | allow |
| `plan` | allow | deny | deny | deny | deny |

Behavior of `ask`:
- In **text mode**: prompts the user interactively on stderr for y/n confirmation
- In **jsonl mode**: automatically denies (no interactive input available)

When an action is denied:
- `approval.requested` and `approval.resolved` (decision=deny) events are emitted
- The tool result status is `denied`
- The run terminates with status `approval_required` and exit code `3`

## Provider Contract

The default (and only production) provider is DeepSeek. Internal test providers (`mock`, `deepseek-fixture`) exist for harness testing but are not user-facing.

### DeepSeek Provider

- Default model: `auto` (main loop uses `deepseek-v4-pro`, auxiliary tasks use `deepseek-v4-flash`)
- Default base URL: `https://api.deepseek.com`
- Transport: OpenAI-compatible Chat Completions (the Responses API is not required by the runtime)
- Thinking mode: explicitly enabled on every request with `thinking.type = "enabled"`
- Default reasoning effort: `max`; supported values are `low`, `high`, and `max` via `reasoning_effort`, `ORCA_REASONING_EFFORT`, or `DEEPSEEK_REASONING_EFFORT`
- Model limits: 1M-token context window and 384K maximum output for
  `deepseek-v4-flash`, `deepseek-v4-flash-vision-exp`, and `deepseek-v4-pro`
- Multimodal input: every model selection accepts ordered text/image blocks
  from ACP plus TUI clipboard images, dragged or pasted paths/`file://` URLs,
  and image file mentions. The vision model receives images directly; `auto`,
  Pro, and Flash first persist a task-aware vision analysis, strip unsupported
  binary blocks from the coding-model request, and fail the complete turn if
  analysis fails.
- TUI clipboard input: background decoding produces fenced `[Image #N]`
  attachments that survive queueing, queue edits, and submission rejection;
  macOS, Linux, Windows, and WSL have native paths, while headless SSH sessions
  use remote image paths instead. Composer previews and submitted message
  thumbnails use terminal-compatible true-color cells; Enter/click opens a
  zoomable and pannable viewer.
- Streaming: SSE with real-time reasoning/content deltas
- Authentication: `DEEPSEEK_API_KEY` (required)
- HTTP retry: 3 attempts with exponential backoff for 429/5xx status codes
- Timeout: 30s connect, 120s request, 300s streaming
- `finish_reason=length` → error (response truncated)
- `finish_reason=content_filter` → error (content blocked)

Response mapping:
- `reasoning_content` → `assistant.reasoning.delta` + `provider.replay.updated`
- `content` → `assistant.message.delta`
- `tool_calls` → parsed into `tool.call.requested` events
- a tool-call response without `reasoning_content` → tool call remains executable, with no fabricated replay state
- errors → `error` event + status `failed`

### Agent Loop

The runtime executes a multi-turn agent loop with no implicit turn ceiling.
Explicit `[budget]` dimensions (`max_turns`, `max_tool_calls`,
`max_cost_usd_micros`, `max_wall_time_ms`) are independently optional:

1. Send conversation to DeepSeek (with system prompt + tool schemas)
2. If response contains tool calls → execute each tool → add results to conversation → next turn
3. If response is a final message → return success
4. If an explicit budget dimension is exhausted → the operation stops with a
   typed `OperationTerminal::Stopped` (exit code 4) after settling the current
   tool and creating a checkpoint; the `session.completed` event carries the
   typed terminal object.

Subagents run the same loop as a child conversation. Synchronous, asynchronous,
and workflow child agents apply one immutable delegation snapshot for the parent
approval/plan mode, active and configured permission profiles, workspace roots,
permission rules, additional working directories, and model selection. A
request-level child model override can replace only the captured model.

Context window management:
- Window size: DeepSeek V4 1M-token context window, compacted at the configured threshold with response reserve
- Compaction threshold: 80% utilization
- Strategy: preserve system message + most recent messages, truncate older history with a marker

### Replay State

`provider.replay.updated` preserves provider-specific context for multi-turn DeepSeek thinking/tool-use flows (`reasoning_content` + tool call IDs). When DeepSeek returns reasoning with a tool call, the next request fully replays that reasoning alongside its assistant tool calls. If DeepSeek omits reasoning for a tool call, Orca executes the call without fabricating replay state. Recorded sessions also restore the latest provider prompt occupancy before a resumed turn; the TUI's typed context revision prevents an older snapshot from replacing a newer legacy context observation, and assistant stream hydration preserves stream-open order without duplicating an identical completed response.

## Configuration

Priority: Environment variables > CLI arguments > config files > defaults.

Config file path: `$ORCA_HOME/config.toml` or `~/.orca/config.toml`. Project overrides can also live at `.orca/config.toml` in the workspace.

Config file fields include `model`, `api_key`, `base_url`, `approval_mode`, permission rules, hooks, MCP servers, and related runtime settings.
