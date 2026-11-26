# Small reliability fixes: tool limit, script logs, MCP instructions, line endings

Date: 2026-11-27. Size: a set of small changes. They came from comparing Orca with DeepSeek's own harness (dsh) and were confirmed in Orca's code. The user asked for all of them to be fixed; the calls below are the implementer-side choices, each with its reason.

## 1. Orca's own tools survive the 128-tool limit

**Today.** `crates/orca-provider/src/deepseek_http.rs` sorts every tool by name, then truncates the list to 128 (`DEEPSEEK_MAX_TOOLS`), and only prints a warning on stderr. MCP tools are named `mcp__<server>__<tool>`, which sorts before `read_file`, `run_script`, `task_*`, `update_plan`, `web_*` and `write_file`. So with many MCP tools, Orca's own tools are the ones removed. The full-screen TUI never shows the stderr line.

**Change.**
- `provider_config_for_agent_loop` (`crates/orca-runtime/src/tool_invocation.rs`) applies the limit after the per-session filtering it already does, under one rule:
  - every tool whose name does not start with `mcp__` is kept (built-in and external tools);
  - MCP tools fill the remaining places in name order;
  - the rest are left out.
- The provider's own truncation stays as a last guard, using the same priority rule, and logs with `tracing::warn!` instead of `eprintln!`, so it cannot draw over the TUI.
- `mcp_startup_warnings` (`crates/orca-runtime/src/mcp_startup.rs`) gains one warning when the connected servers offer more MCP tools than fit beside Orca's own tools.
  - The places available are 128 minus the number of tools in Orca's default registry.
  - The warning names the count left out and the first few names, and suggests `disabled_tools` (or `enabled_tools`) in that server's configuration.
  - `orca exec` and the TUI already show these warnings.

**Why not the alternatives.** Sorting MCP tools last in the request would change the bytes of every session's tool list for nothing, and keeping the sort and choosing what to drop is enough. On-demand MCP loading is the larger fix and is planned separately.

## 2. A stopped script keeps what it logged

**Today.** In `crates/orca-runtime/src/script/prelude.js`, `log` pushes lines into an array that reaches Rust only when `main` settles. When the 60 s wall clock or the memory limit stops a script, `engine.rs` returns only `the script ran longer than 60s and was stopped` or `the script used more than 64 MiB`. Everything the script logged is lost, and the model reruns the whole script.

**Change.**
- Each `log` call also hands its line to the host at once, through a native binding next to the existing call bridge.
- When the wall clock or the memory limit stops the script, the failure text becomes the current message plus `\n\nLogged before it was stopped:\n<lines>`. This is the same shape a thrown error already uses (`Logged before the error:`).
- The kept lines are bounded by the same output limit the completed result uses.
- A cancelled script still returns the interruption error: the turn is ending, so the lines would reach no one.

## 3. MCP server instructions reach the model

**Today.** The MCP `initialize` result may carry an `instructions` string, where a server explains how its tools should be used. `crates/orca-mcp/src/client.rs` reads only the protocol version and `capabilities`, so the model never sees those instructions.

**Change.**
- orca-mcp keeps each server's non-blank `instructions` from its last successful `initialize`. `McpRegistry` exposes them in server order.
- The runtime puts them in a new internal-context fragment, `mcp`, set at turn start the way the memory fragment is:
  - each server's text is under the heading `Instructions from MCP server '<name>':`;
  - the fragment is capped at 4,096 tokens (`MCP_CONTEXT_MAX_TOKENS`), like the other capped fragments;
  - when no server has instructions, the fragment is absent.

**Why a fragment and not the system prompt.** Servers start in the background and can reconnect. A fragment follows them without rewriting the cached system prompt, and the system prompt has a size budget that a server's text must not consume.

## 4. Edits keep working on CRLF files

**Today.**
- `read_file` with a line range drops the `\r` before each `\n` (`crates/orca-tools/src/read_file.rs:164`). The model therefore writes `old_text` with bare `\n`.
- `edit` matches bytes exactly (`crates/orca-tools/src/edit.rs:77`), so a multi-line edit of a CRLF file fails with `edit old text was not found`.
- `write_file` writes the model's LF text over a CRLF file, converting the whole file.

**Change.**
- `edit` finds the file's line ending by majority: CRLF when `\r\n` pairs outnumber bare `\n`.
  - For a CRLF file, it matches with the file's text and both `old_text` and `new_text` normalized to `\n`.
  - It writes the result back with `\r\n` line endings.
  - Uniqueness and not-found errors are counted on the normalized text.
  - An LF file behaves exactly as today.
- `write_file` over an existing CRLF-majority file converts the new content's bare `\n` to `\r\n`, unless the content already contains `\r\n`.
- New files are written as given.

**Considered and dropped: atomic writes.** Writing through a temporary file and a rename would protect against a write cut off midway. But it replaces a symlink or breaks a hard link instead of writing through it, and on Windows it fails while another program holds the file open. Editors hold files open all the time, so this would break edits users make today. Writes stay in place.

## 5. Goal-mode doc matches the code

`docs/goal-mode.md` says the internal-context fragments are sent "after canonical instructions and before conversation history". `render_internal_context` is appended as the last message of the request (`crates/orca-provider/src/deepseek_http.rs:1214-1226`). The doc is corrected to say so, including why: a change then costs only that message instead of the cached history.

## Non-goals

- No change to any tool's model-facing description or schema, except where a fix needs it.
- No on-demand MCP loading, no change to how the internal context is cached. Both are separate later work.

## Constraints

- The repository is public; describe each change by what it does for a user.
- Windows compiles only in CI. If an edit adds a `std::os::unix::` use or moves another pinned count, update `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json` in the same commit.
- Adding a fragment id or a field to a recorded type may touch the runtime-surface contract. Run `node scripts/validate-runtime-surface-contract.mjs` and its test, and refresh pins exactly as that validator's errors say.
