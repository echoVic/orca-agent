# Workspace Reads Stay in the Workspace

## Decisions taken with the user on 2026-11-28

1. **D1.** `grep` searches in process on every platform. Walk directories in parallel, check verified ignore rules before descending, and open files relative to verified directories. Sort native paths before applying `offset` and `head_limit`. Keep bounded output and cancellation. Decode BOM-marked UTF-16LE, UTF-16BE and UTF-8 and preserve decoded line numbering. Recursive discovery skips links, hidden entries, binary files and unreadable files. Explicit targets can resolve to files or directories inside the workspace.
2. **D2.** Read `.gitignore` and `.ignore` from the workspace through its Git root, including intervening ancestors. Open each without following links. Deeper rules win within each file type. `.ignore` takes precedence over `.gitignore`, and negation cannot reopen an excluded parent. Strip a UTF-8 BOM from the first line. The global excludes file and associated repository `info/exclude` are the other outside sources. Reading `core.excludesFile` can consult system, global and XDG Git configuration.

## Read behaviour

- Pin the canonical workspace root and its directory identity on first use. Refuse later calls when that root changes. Resolve the request inside the pinned root, then verify components when opening files. Links that resolve inside the workspace work for explicit targets. Hard links are ordinary files. Treat a drive-relative Windows path as invalid. Accept normal and verbatim Windows paths and equivalent macOS root spellings after checking their native identity on disk.
- An empty path means the workspace root. Validate intermediate components before applying `..`. A missing intermediate directory or a file used as one is an error.
- For `glob` and `list_files`, each returned entry was inside the workspace when it was checked. Enumerate from verified directory handles and recheck the selected page before returning it.
- Verify a linked worktree through the backlink in the common directory's `worktrees/<name>/gitdir`. For a separate Git directory, read its `info/exclude` when the Git directory is inside the workspace or its own `core.worktree` points back to the worktree. Use the same verified directory handle for the association and the excludes read.
- Model Workflow paths resolve inside the workspace, a trusted ancestor's `.orca/workflows`, or the pinned user workflow directory. A saved `name` is a single component. A draft ID must be a single safe component, and verified draft reads are relative to the session root. The Workflow host hashes the bytes it loads and refuses a mismatch with the prepared script. User commands can still pass direct script paths.
- Edit and Apply Patch continue to resolve their write targets through the existing write resolver. Spill reads continue to use the session's spill directory with verified opens.
