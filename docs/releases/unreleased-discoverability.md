# Skill and MCP tool discovery

- The model sees a catalog of available skills at the start of each turn. A
  `$skill` mention keeps the body in the conversation until compaction. Orca
  finds project skills from the Git root and gives them precedence over user
  skills with the same ID.
- MCP servers can opt into `tool_loading = "on-demand"`. Orca sends a catalog
  and a loader first. When loading changes the tool list, the next request
  misses the prompt cache once. Saved sessions restore the loaded selection.
