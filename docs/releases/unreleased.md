# Unreleased

- Orca saves large tool results as session files. The model can page through the full text with `read_file` or search it with `grep`, even after the result preview is reduced during compaction.
- Orca discovers nested `AGENTS.md` files in trusted workspaces as it reads or changes files. User-wide, project-root, and nested instructions have independent byte limits.
- **Skill discovery.** The model sees a catalog of available skills at the start of each turn. A `$skill` mention keeps the body in the conversation until compaction. Orca finds project skills from the Git root and gives them precedence over user skills with the same ID.
- **On-demand MCP tools.** MCP servers can opt into `tool_loading = "on-demand"`. Orca sends a catalog and a loader first. When loading changes the tool list, the next request misses the prompt cache once. Saved sessions restore the loaded selection.
- **Multi-file patches.** Set `apply_patch = true` under `[tools]` to let Orca add, update, delete, and move workspace files in one tool call. Orca checks the whole patch before writing. The flag is off by default.
- **Tool definitions and prefix caching.** Enabling `apply_patch` adds one tool definition to the model request. The change in tool list bytes can cause a cache miss on the first request with the new list.
- **Fork subagents.** Use `context: "fork"` to start a child with the parent's completed DeepSeek history, tool list, model, and reasoning effort. The child can reuse that request prefix and reports its own work. The default `context: "fresh"` starts from a standalone brief.
- **Tool guidance.** `bash` describes how to set a working directory, handle sandbox refusals, and guard paths built from shell variables. `edit` explains when to read first, and `read_file` names the shell reads it replaces. The changed tool definitions cause one prompt-cache miss per session on its first request after upgrading.

A shortened result or newly discovered instruction changes the prompt from that point onward. The first request with the changed prefix can miss the provider's prompt cache.
