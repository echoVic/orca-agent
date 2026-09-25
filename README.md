# Orca

A DeepSeek-native coding agent for your terminal.

Give Orca a task and it reads code, edits files, runs commands, verifies the
result, and keeps working until the task is done or it needs you. Use the TUI
for interactive work or `orca exec` for scripts and CI. Orca is built in Rust,
runs locally, and is MIT licensed.

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Website](https://orcaagent.dev/) · [Changelog](https://orcaagent.dev/changelog/) · [Releases](https://github.com/echoVic/orca-agent/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## Install

```bash
npm install -g @blade-ai/orca
```

Or install the native binary directly:

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

On Windows PowerShell:

```powershell
irm https://orcaagent.dev/install.ps1 | iex
```

From a project directory, provision its restricted sandbox capability with:

```powershell
& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox
```

The npm package supports macOS, Linux, and Windows on ARM64 and x64. Prebuilt
archives are also available from [GitHub Releases](https://github.com/echoVic/orca-agent/releases/latest).

On Windows, Orca prefers PowerShell 7 and detects its standard installation
path even when it is absent from `PATH`. Restricted sessions fall back to
`cmd.exe` when PowerShell 7 is unavailable. Windows PowerShell 5.1 remains an
explicit option only for modes that do not require AppContainer isolation.
Protocol command arrays are launched as native Windows argv without shell
re-parsing; legacy string commands use the resolved shell dialect.

## Use

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # open the terminal UI
orca exec "fix the failing test"          # run headlessly
printf '%s' "$INSTRUCTION" | orca exec   # keep arbitrary prompt text out of argv
orca exec --verifier "cargo test" "fix it" # verify before finishing
orca exec resume SESSION_ID "continue"    # resume a headless session
orca exec resume --last "continue"        # resume the most recent session
orca exec resume SID --resume-at MID "continue"  # resume up to a message boundary
orca --resume [SESSION_ID]                # resume a saved conversation
orca --fork SESSION_ID                    # fork a saved conversation
orca --mode=acp                           # connect an ACP client
orca doctor                               # check the key, trust, and sandbox locally
```

On Windows PowerShell, set the key with `$env:DEEPSEEK_API_KEY = "sk-..."`;
the `orca` commands are the same. The positional prompt form remains
supported. When the prompt may contain tokens that a task later searches for or
kills in the process table, pipe it on stdin so it is not exposed in `orca`'s
command line.

Orca also offers opt-in [shared ACP sessions](docs/acp-daemon.md) on Unix
(`orca daemon`, `orca attach`, and `orca acp-bridge`),
[file-defined subagents](docs/subagents.md), and
[persistent terminal output pages](docs/architecture/adr/0006-unified-exec-terminal-service.md).

### The terminal UI

Run `orca` in a project. The first run in a folder asks you to trust it or
continue untrusted: trust lets Orca load the project's configuration,
instructions, skills, agents, and workflows, and never enables or bypasses the
OS sandbox. Then type a task and press `Enter`.

- **Mention and command.** `@` mentions files, skills, plugins, and MCP
  resources; `$` inserts a skill; `/` opens the command menu; `?` lists every
  key. `Ctrl+V` attaches a clipboard image.
- **Follow along.** Replies are marked `●`, reasoning collapses to one
  `⋯ thinking` line, and each tool call shows its output under a `│` rail.
  `e` expands the latest collapsed output and `Shift+E` all of them. The
  status bar shows the approval mode, the model and reasoning effort, the
  context left, and usage.
- **Steer a running turn.** `Esc` interrupts. `Enter` queues a follow-up for
  the next turn, `Ctrl+Enter` sends it into the running turn (in terminals
  with the kitty keyboard protocol), and `Ctrl+B` moves the turn to the
  background.
- **Approve tool calls.** A call that needs approval turns the input into an
  approval panel: allow once, allow this exact call, allow the tool for the
  session, or deny (`Esc`). `Shift+Tab` cycles `suggest` → `auto-edit` →
  `full-auto` → `plan`. Entering `full-auto` asks for an explicit Full Access
  confirmation; the running task picks it up at its next tool call, while tools
  already running and subagents already launched keep their original policy.
  Mode changes last for the session and are never saved.
- **Background work.** `/tasks` shows the tasks dock under the conversation,
  where background turns, subagents, commands, monitors, and workflow children
  appear. `/agents` opens the Agent Workspace with each task's live
  conversation or transcript and the controls it can safely take: stop,
  resume, retry, or a follow-up. When background agents finish while you are
  idle, Orca continues with their results. `/workflows` keeps the workflow run
  tree.
- **Plan, goals, and recap.** `/plan` investigates read-only and ends with a
  plan to approve; `/goal` sets a persistent objective; `/recap` summarizes the
  session, and Orca writes one itself when you return after a quiet spell;
  `/side` opens a side conversation for a quick question.
- **Sessions.** `/new`, `/resume` (grouped by project, with fork, rename,
  archive, delete, and copy ID), `/fork [name]`, `/rename [name]`, `/model`,
  `/config`, and `/copy [N]`. Model and reasoning-effort choices are saved to
  the user `config.toml`, so new sessions use them too. `/status` reports the effective execution profile,
  shell sandbox, and permission profile. `Ctrl+L` clears the screen but keeps
  the conversation, and on exit Orca prints the `orca --resume <SESSION_ID>`
  command.

The [Terminal UI guide](https://orcaagent.dev/docs/#terminal-ui) walks through
the screen and every key.

Automatic project memory is on for recorded sessions; use `/remember` for
explicit user or project facts. See [Memory](docs/memory.md) for capture,
recall, storage, privacy, and deletion.

### Use Orca from Pilion Browser

[Pilion Browser](https://github.com/echoVic/pilion-browser) is a desktop browser that works as an ACP client. Choose **Orca** in its Agent panel: Pilion launches `orca --mode=acp`, forwards `DEEPSEEK_API_KEY`, and exposes its own tabs to Orca as MCP tools (`browser_snapshot`, `browser_screenshot`, navigate, click, type) with approval-before-action and human takeover. Installers for macOS, Windows and Linux are on the [Pilion releases page](https://github.com/echoVic/pilion-browser/releases).

## What it does

- Uses DeepSeek's reasoning and tool-use semantics directly, with SSE streaming,
  prefix-cache-friendly prompts, automatic context management, and retry logic.
- Reads, searches, edits, and writes code; runs shell commands; and can verify
  the result with a command you choose. `bash` is the only command entry point,
  and every command it starts belongs to the task rather than to the tool call:
  a long build, a CI watch, or an interactive PTY session keeps running after
  the call returns. `yield_time_ms` bounds only how long the call waits, while
  `timeout_ms` is the sole caller-side execution deadline. `task_read_output`,
  `task_send_input`, `task_wait`, and `task_stop` continue, feed, wait for, and
  stop a command by its `task_id`. A background supervisor settles exited or
  stopped sessions without polling and injects one bounded completion
  notification before the next model turn.
- Asks one to four structured clarification questions in interactive TUI
  sessions, including described choices, optional previews, and multi-select
  answers.
- Gates risky actions with `suggest`, sandboxed `auto-edit`, full-access
  `full-auto`, and read-only `plan` modes, plus per-folder trust.
- Saves local conversations with `--resume` for continuation and `--fork` for
  branching; `orca exec resume <SESSION_ID>` restores a headless session with a
  fresh budget scope, and headless exits print the exact resume command.
- Gives synchronous subagents, async subagents, and workflow child agents a
  runtime-owned continuation id. A later `subagent` call can pass
  `resume_from` with that continuation id (or the originating task id) to append
  a new prompt to the same durable child conversation. Task/status output on
  TUI, ACP, JSONL, and headless surfaces includes the current attempt,
  checkpoint, resumable, and indeterminate state.
- Runs direct, nested, Workflow, hosted, continued, and recovered children
  through one durable execution scope per root task tree. The default 32
  execution leases are a capacity ceiling rather than a delegation target;
  accepted overflow queues without creating a worker, and parents waiting for
  children yield their lease before re-entering the fair queue.
- Keeps up to four active child summaries visible in the conversation and up to
  eight durable activity entries per child. `/agents` opens live conversations
  and transcripts and exposes only controls the selected child can safely
  perform: stop, resume, retry, or a revision-fenced follow-up.
- Learns a bounded set of durable project facts after successfully committed
  turns and retrieves only prompt-relevant facts on later turns.
- Runs with no implicit turn ceiling; optional `[budget]` limits
  (`--max-turns`, `--max-tool-calls`, `--max-cost-usd`,
  `--max-wall-time-secs`) bound an operation explicitly, and budget stops
  settle the current tool, create a checkpoint, and exit 4 with a typed
  terminal in the JSONL stream.
- Runs persistent goals without a fixed turn ceiling (a cumulative Goal token
  budget disables automatic continuation when exhausted), plus subagents and
  JavaScript workflows for longer tasks that need continuation or parallel work.
- Loads project instructions, skills, plugins, custom tools, MCP tools, and MCP
  resources after the workspace is trusted.
- Exposes stable JSONL, app-server, and Agent Client Protocol (ACP) contracts
  for editors, harnesses, and CI.

Configuration priority is environment variables, CLI arguments, config files,
then defaults. Run `orca --help` or `orca exec --help` for the full command
surface. User configuration lives at `~/.orca/config.toml`; trusted projects
can also provide `.orca/config.toml`, `AGENTS.md`, rules, skills, and workflows.

An unset or `auto` model uses `deepseek-flash` (DeepSeek-V4.1-Flash). Choose
`deepseek-v4-pro` with `/model`, `--model`, or `model = "deepseek-v4-pro"` in
`config.toml`; releases before 0.5.0 routed `auto` to Pro. Both models use a
1M-token context window and allow up to 384K output tokens. The retired
`deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` names remain accepted
and normalize to `deepseek-flash`, including Flash pricing. DeepSeek thinking is
enabled explicitly: set `reasoning_effort` to `low`, `high`, or `max` (the
default) in `config.toml`, or use `ORCA_REASONING_EFFORT`.

JPEG, PNG, GIF, and WebP inputs are accepted from ACP clients and the TUI with
every model selection. Flash consumes images directly; Pro uses Flash for
task-aware visual analysis before continuing with Pro. In the TUI, use `Ctrl+V`
to attach the current clipboard image (`Alt+V` is also available on Windows),
drag or paste image paths and `file://` URLs, or select an image through
`@file`. Each attachment appears as an atomic `[Image #N]` item that can be
deleted, cleared, queued, edited, and restored after a rejected submission.
`Cmd+V` works when the terminal forwards it as `Super+V`; terminals that
consume `Cmd+V` should use `Ctrl+V`. Clipboard reads run in the background, and
pressing Enter while one is pending waits for the image before submitting.
Place the cursor on an image item and press Enter to open its preview;
submitted images also render in the message area and can be clicked. The viewer
supports `+`/`-` zoom, arrow-key panning, `0` to fit, and Esc to close. Kitty
and Ghostty use the Kitty graphics protocol for native-pixel previews; iTerm2
and WezTerm use the inline-image protocol. Terminals without an image protocol,
including Apple Terminal, fall back to low-resolution true-color cells. SSH
sessions without a graphical clipboard should paste or mention a path on the
remote host. Inline attachments share a 5 MiB total limit. Orca keeps the Chat
Completions transport and fully replays any returned `reasoning_content` across
tool turns as required by DeepSeek.

More detail:

- [Documentation](https://orcaagent.dev/docs/) and the
  [Terminal UI guide](https://orcaagent.dev/docs/#terminal-ui)
- [Persistent Goal Mode](docs/goal-mode.md)
- [Memory](docs/memory.md)
- [Harness and app-server contract](docs/harness-contract.md)
- [Dynamic workflow design](docs/claude-code-workflow-parity.md)
- [Production roadmap](docs/production-roadmap.md)

## Reliability

- TUI, headless, ACP, and JSONL sessions use the same runtime host for turn
  ownership, cancellation, persistence, and terminal results.
- Goal and session storage run outside the async actor loop, so a slow disk or
  busy SQLite database does not freeze unrelated controls such as cancel or
  status.
- Cancelling a foreground turn also stops the subagent task tree it owns;
  unrelated detached work is left alone.
- Escape-driven cancellation commits one terminal child state and ignores late
  activity from the cancelled attempt, so a stopped subagent cannot flood the
  terminal while its parent returns to an interactive prompt.
- Continuation recovery is deliberately fail-closed. Orca restores only a
  digest-verified conversation checkpoint, never a Rust future or process
  stack. A tool admitted with unknown external side effects makes the
  continuation `indeterminate` until a later safe checkpoint covers its
  terminal result. Worktree continuations inherit the original path only while
  it still exists; retryable resumable attempts retain that path, and Orca does
  not silently recreate a missing worktree.
- The durable prompt queue and checkpointable child-agent continuation model
  project the same queued, resumable, indeterminate, and terminal state across
  TUI, ACP, JSONL, and Headless. Large ordinary-chat pastes remain compact in
  the composer but submit their complete text; Goal pastes are materialized
  under `ORCA_HOME/attachments/<uuid>` with path validation and transactional
  cleanup before the Goal mutation commits. Alt+Up queue editing commits a
  revision-checked runtime delete, failed queue admission restores the prompt,
  and queued previews remain bounded instead of copying the full body per frame.
- Detached background workers never hold the terminal open, and they end once
  their task is gone or their lease is lost.
- Session switches start the replacement before closing the current runtime.
  Rename, fork, archive, and delete commit through revision-checked and durable
  paths, and stale events from a previous attachment are ignored.
- Runtime surface and platform contracts run in CI before release artifacts are
  built for macOS, Linux, and Windows.

## Community

- QQ group: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before contributing. Open an issue first
for large or compatibility-sensitive changes.

- [Report a bug](https://github.com/echoVic/orca-agent/issues/new?template=bug_report.yml)
- [Request a feature](https://github.com/echoVic/orca-agent/issues/new?template=feature_request.yml)
- [Ask for help](SUPPORT.md)
- [Report a vulnerability](SECURITY.md)

## License

[MIT](LICENSE)
