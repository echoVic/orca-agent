# Unreleased

## Fixes

- `read_file`, `read_document`, `grep`, `glob`, and `list_files` keep reads and results inside the workspace when paths change during a call. `grep` skips symlinked ignore files and honors the global Git excludes file and the repository's `info/exclude`. The `Workflow` tool accepts saved names and script paths inside the workspace or workflow directories.
