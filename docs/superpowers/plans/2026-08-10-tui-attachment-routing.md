# TUI Attachment Routing Plan

- [x] Identify the renderer-owned attachment event orchestration seam.
- [x] Extract relay, stale-event filtering, parent status, and deferred interaction delivery.
- [x] Keep the existing stale-session and relay-generation behavior tests against the extracted API.
- [x] Run focused TUI tests, PTY contract, surface-contract validation, and formatting checks.
- [x] Review the activation/FIFO boundary and add regression coverage for both races.
- [x] Commit the independent slice.
