# Large tool output goes to a file; skills, MCP tools and nested instructions are discoverable

Date: 2026-11-27. Size: project (four parts that can ship separately). It draws on DeepSeek's harness (dsh); the user asked for these learnings to be adopted.

## 1. A tool result too large for the context is saved to a file

**Today.**
- Built-in tools bound their own output, but differently. grep pages; `web_fetch` cuts at `max_bytes` with no paging arguments.
- MCP and external tool text is not bounded at all (`mcp_call_result_to_tool_result` in `crates/orca-tools/src/registry.rs`), so one call can add an arbitrarily large result to history permanently.
- Older outputs are later trimmed to 8 KiB head and tail (`[... tool output micro-compacted ...]`), and the middle is lost.
- A 12.5K-token spill threshold was planned in `docs/superpowers/specs/2026-09-29-context-budget-design.md` §8, but never built.

**dsh.** A result over `maxInlineTokens` (12,500) is saved whole.
- The model gets the head and tail in order, then: `(Omitted N bytes. Full formatted result stored at: <path>. Use read with offset/limit, or grep this path to search within it.)`
- Results of its own `read` tool are never spilled.
- A storage failure keeps the original result.

**Change.**
- **Which results are saved.** A completed tool result whose text exceeds 12,500 tokens is saved whole under the session's own directory in `ORCA_HOME`, one file per tool call, readable only by the user. This applies to MCP tools, external tools, `web_fetch` and `web_search`. `read_file`, `grep`, `glob`, `task_read_output` and shell tasks are never saved, because they already page.
- **What the model gets.** The head and tail within the 12,500-token budget, then the notice:
  `[{omitted} bytes omitted. The full result is saved at {path}; read it with read_file using offset and limit, or search it with grep.]`
- **The paths must be readable.** `read_file` and `grep` accept paths under the session's spill directory as a read-only exception to the workspace rule. That directory and nothing else; no writes.
- **The micro-compaction marker** of a saved result keeps the path, so the full text stays reachable after it is trimmed.
- **Clean-up.** Spill files are deleted with their session. A storage failure keeps today's behaviour, with no spill.

## 2. The model knows which skills exist

**Today.**
- The system prompt never mentions skills. The model learns of one only through `list_skills`, which its own description says to call "when the user asks for a skill", or through a `$id` mention.
- A mentioned skill's body lives in the `skill` fragment, and three things go wrong with it:
  - the next prompt replaces the fragment, so it disappears on "continue";
  - it is cut silently at 4,096 tokens;
  - it misses the cache on every request.
- `project_root` stops at the first `Cargo.toml` or `package.json` (`crates/orca-tools/src/skills.rs`), so a session started in a sub-crate misses the repo root's `.orca/skills`.
- User skills shadow project skills with the same id.

**Change.**
- **A catalog fragment.** A new internal-context fragment, `skills`, holds a catalog. Each skill is listed as `` `name`: description ``, sorted, with the description cut at 500 characters. After the list:
  `When a task matches one of these skills, load it with read_skill before you act.`
  It is capped at 4,096 tokens; past the cap it ends with `({n} more skills; list_skills shows them all)`.
- **A `$id` body is kept.** A mentioned skill's body is appended to history once, as a system message, rather than held in the per-prompt fragment. It then stays until compaction like any message. A body over the cap ends with `[skill text cut at {n} tokens; read_skill {id} shows the rest]`.
- **The project root** is the nearest directory with `.git`, or the workspace root when there is none.
- **A project skill wins** over a user skill with the same id. The release note says so.

## 3. MCP servers can offer their tools on demand

**Today.** Every tool of every connected server is sent on every request, and nothing outside tests uses `ToolExposure::Deferred`. One large server (Notion's 46 tools, about 214,500 characters of schema) fills the window early.

**dsh.** A server set to `toolLoading: on-demand` registers only `mcp__<server>__load_tools`, plus a catalog of tool names with the first line of each description.
- A load validates every name before registering any.
- Loaded tools are scoped to the calling agent.
- Reconnects re-register what was loaded.
- A server tool literally named `load_tools` is excluded with a warning.

**Change.**
- **The setting.** MCP server configuration gains `tool_loading = "eager" | "on-demand"`, default `"eager"`.
- **An on-demand server is offered as one tool and a catalog.**
  - The tool is `mcp__<server>__load_tools` with `{"names": [string]}`.
  - The catalog sits in the `mcp` fragment, under `On-demand tools from MCP server '<server>':`, one line per tool: `name — first line of its description`, cut at 160 characters.
- **Loading.**
  - It validates all names first; an unknown name fails the whole call and lists the valid names.
  - Loaded tools join the session's tool list from the next request on. They are recorded so a resume re-registers them, and the server's reconnect keeps the ones it still offers.
  - Subagents start with none loaded.
- **The release note says** that loading changes the tool list, so the next request misses the prompt cache once.

## 4. Instructions in subdirectories are found, and instructions are bounded

**Today.** Orca loads `~/.orca/AGENTS.md`, the trusted folder's root `AGENTS.md` and `.orca/rules/*.md` once per session into the system prompt (`crates/orca-runtime/src/instructions.rs`). An `AGENTS.md` in a subdirectory is never seen, and nothing bounds the total size.

**Change.**
- **Nested files.** When a successful `read_file`, `write_file` or `edit` touches a file below a directory that holds an `AGENTS.md`, between the workspace root and the file, that file's text is appended once, before the next request, as a system message:
  `Additional instructions for {dir}/ from {dir}/AGENTS.md:`
  A later change to the file appends `Updated instructions …`.
- **Budgets.**
  - Each chain has its own budget of 65,536 bytes: user-global, project root, and nested.
  - Past a budget, the broadest files are dropped first and the most specific is cut last, with a note naming what was left out.
  - The project root's chain keeps today's content when it fits.

## Non-goals

- No recall tools over compacted history.
- No automatic switch to on-demand MCP loading.

## Constraints

- Each part ships with its own tests and docs (site docs in English and Chinese), and its own release-note entry.
- Prompt and schema changes alter cache bytes once, and the release notes say so.
