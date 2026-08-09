# Server JSONL Tool Approval Attachment Plan

## 1. RED: lock the missing capability at the attachment boundary

- Add a unit-level helper/assertion around server turn attachment capabilities.
- Run the focused test and capture the failure showing `ToolApproval` absent.

## 2. GREEN: declare tool approval on turn attachments

- Add `SurfaceInteractionKind::ToolApproval` to persistent and stateless JSONL
  turn attachment requests.
- Keep the real server harness honest about `suggest` mode: accept and verify a
  second, tool-owned `permission_request` before waiting for the terminal.
- Keep read-only/control/settings attachment capability sets unchanged.
- Run focused tests, runtime lifecycle tests, formatter, and diff checks.

## 3. Real verification and integration

- Rebuild the binary and rerun real provider/TUI/ACP/server gates as needed,
  with emphasis on server approval recovery.
- Review the diff for ownership, protocol, and scope regressions.
- Rebase onto the latest `origin/main`, integrate the reviewed commit, and
  rerun the post-merge focused gate.

## Completion Evidence

- RED: `server_turn_attachments_route_tool_approval` failed because the shared
  server turn capability set omitted `ToolApproval`.
- GREEN: the focused adapter tests passed 3/3; the broader runtime server slice
  passed 128/128; and `runtime_lifecycle_contract` passed 54/54.
- `node --check scripts/release/real-api-server-approval-recovery.mjs` and the
  harness self-test passed.
- The rebuilt binary completed the real DeepSeek server flow and printed
  `ORCA_SERVER_APPROVAL_RECOVERY_REAL_OK` after the initial permission request,
  routed `write_file` approval, durable output, EOF handling, and restart
  recovery checks.
- `cargo fmt --all -- --check` and `git diff --check` passed.
