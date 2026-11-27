# Unreleased

- Orca saves large tool results as session files. The model can page through the full text with `read_file` or search it with `grep`, even after the result preview is reduced during compaction.
- Orca discovers nested `AGENTS.md` files in trusted workspaces as it reads or changes files. User-wide, project-root, and nested instructions have independent byte limits.

A shortened result or newly discovered instruction changes the prompt from that point onward. The first request with the changed prefix can miss the provider's prompt cache.
