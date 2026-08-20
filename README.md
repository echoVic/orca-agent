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

orca                                      # open the TUI
orca exec "fix the failing test"          # run headlessly
orca exec --verifier "cargo test" "fix it" # verify before finishing
orca exec resume SESSION_ID "continue"    # resume a headless session
orca exec resume --last "continue"        # resume the most recent session
orca exec resume SID --resume-at MID "continue"  # resume up to a message boundary
orca --mode=acp                           # connect an ACP client
orca --resume [SESSION_ID]                # resume a saved conversation
orca --fork SESSION_ID                    # fork a saved conversation
```

On Windows PowerShell, set the key with `$env:DEEPSEEK_API_KEY = "sk-..."`;
the `orca` commands are the same.

In the TUI, `@` searches files, skills, plugins, and MCP resources. Session
commands are `/new`, `/resume`, `/fork [name]`, `/rename [name]`, `/status`,
`/config`, and `/copy [N]`. `/config` opens an interactive session settings
panel. The resume picker also supports fork, rename, archive, delete,
and copying a session ID. `/history` is retired; `/clear` remains a hidden
compatibility alias for `/new`. `Ctrl+L` clears only the displayed transcript
and terminal scrollback, keeping the current conversation context. On exit,
Orca prints the exact `orca --resume <SESSION_ID>` command for the session.

Use `/plan` for read-only planning, `/goal` for a persistent objective,
`/workflows` for background work, and `/trust` to manage the current folder's
sandbox permissions. Automatic project memory is enabled for recorded sessions
by default; use `/remember` for explicit user or project facts. See
[Memory](docs/memory.md) for capture, recall, storage, privacy, and deletion.

## What it does

- Uses DeepSeek's reasoning and tool-use semantics directly, with SSE streaming,
  prefix-cache-friendly prompts, automatic context management, and retry logic.
- Reads, searches, edits, and writes code; runs shell commands; and can verify
  the result with a command you choose. Runtime-owned `exec_command` sessions
  can stay alive across tool calls, allocate a PTY, and receive input through
  `write_stdin` for editors, REPLs, and terminal UIs. A background supervisor
  settles exited or stopped sessions without polling and injects one bounded
  completion notification before the next model turn.
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

DeepSeek V4 thinking is enabled explicitly. Set `reasoning_effort` to `low`,
`high`, or `max` (the default) in `config.toml`, or use
`ORCA_REASONING_EFFORT`. Both `deepseek-v4-flash` and `deepseek-v4-pro` use a
1M-token context window and allow up to 384K output tokens. Orca keeps the
Chat Completions transport and fully replays any returned `reasoning_content`
across tool turns as required by DeepSeek.

More detail:

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
