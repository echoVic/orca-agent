# Unreleased

## Fixes

- Orca can reopen a session and run other tasks when a fork child’s history is damaged. If Orca cannot load a new fork’s history, you can retry with the same continuation ID.
- In the terminal UI and app-server sessions, children launched with `context: "fork"` now start from the parent’s history. They previously could start fresh without a note.
