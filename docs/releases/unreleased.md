# Unreleased

## Fixes

- `grep` searches in process on every platform and no longer uses ripgrep. The matches for ordinary repositories stay the same, and path sorting keeps pages stable. Ignore rules from the workspace's Git ancestors still apply. The shorter tool description causes one prompt-cache miss per session after upgrading.
- Workspace read tools refuse outside paths, directories replaced by links during open, and workspace roots replaced after first use. `glob` and `list_files` recheck entries before returning them. Hard links inside the workspace are treated as ordinary files.
- Workflow drafts read their metadata and script from the session directory and refuse invalid draft IDs. The Workflow host refuses to run a script whose bytes changed during startup.
