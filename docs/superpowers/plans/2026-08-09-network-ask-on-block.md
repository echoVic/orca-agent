# Network Ask-on-Block

## Plan

- [x] Add an integration test that starts a thread with an active network
  allowlist, runs a command without repeating `permissionProfile`, and proves
  the permission request precedes completion.
- [x] Route network block reports whenever the effective sandbox has a
  requestable network policy and a thread-owned permission request can be
  constructed.
- [x] Verify the allow/retry/persist path and preserve denylist behavior.
- [x] Run focused and full runtime gates, format/diff checks, and review the
  independent commit.

## Evidence log

Initial source evidence: `run_command_exec` builds `effective_sandbox` from
explicit or inherited profiles, but initially created `retry_block_receiver`
only when `options.permission_profile.is_some()` (server.rs around lines
1330-1350). The regression test failed with a terminal empty completion and no
permission request; the effective-policy route now passes.

Final verification: server unit tests passed 127/127, the full runtime library
passed 1033/1033, `cargo check -p orca-runtime --lib` passed, and format/diff
checks passed. Independent review found no P0/P1 defects.
