# Session Network Grant Fixture Plan

## Goal

Make the session-scoped command/exec network-grant contract deterministic without changing runtime policy.

1. [x] Replace the external `api.example.com` request with a test-owned loopback HTTP listener and update the requested/persisted domain assertion to `127.0.0.1`.
2. [x] Run the focused test and confirm the local listener, persisted grant, retry completion, and later request/deny settlement are all observed.
3. [x] Run related server tests plus `cargo fmt --all -- --check` and `git diff --check`.
4. [x] Commit the test-only repair, rebase onto current `main`, and rerun the focused test.

## Evidence

- The old fixture failed deterministically on 2026-08-09 because `api.example.com` resolved to `198.18.9.23`, which the proxy correctly blocks as benchmark/private address space.
- The updated focused integration test passes and proves the loopback retry, thread/read and thread/list persistence, later permission request, and explicit deny settlement.
- Neighboring `server_mode_session_network_deny_overrides_permission_profile_allow` and `server_mode_command_exec_inherits_thread_active_permission_profile` tests pass.
- `cargo test -p orca-runtime server::tests::command_exec_ --lib --locked -- --test-threads=1` passed 36/36.
- CodeRabbit raised one major fixture-lifecycle concern. The valid accept-hang portion was fixed with a bounded nonblocking loop; the suggested `--noproxy 127.0.0.1` change was rejected because it would bypass the runtime proxy and invalidate the permission test.
- The CodeRabbit rerun raised 0 issues.
