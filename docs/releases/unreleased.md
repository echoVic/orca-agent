# Unreleased

- **Multi-file patches.** Set `apply_patch = true` under `[tools]` to let Orca add, update, delete, and move workspace files in one tool call. Orca checks the whole patch before writing. The flag is off by default.
- **Tool definitions and prefix caching.** Enabling `apply_patch` adds one tool definition to the model request. The change in tool list bytes can cause a cache miss on the first request with the new list.
