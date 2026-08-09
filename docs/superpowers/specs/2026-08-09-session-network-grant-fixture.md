# Session Network Grant Fixture

## Problem and Evidence

`server_mode_command_exec_uses_session_network_domain_grants` is intended to prove that an allowed session-scoped network domain grant is persisted, applied to the retried `command/exec`, and remains visible through `thread/read` and `thread/list`. The test sends its retried command to `api.example.com`.

On 2026-08-09, local DNS resolved that name to `198.18.9.23`. The runtime correctly treats `198.18.0.0/15` as a benchmark/private target and rejects the resolved address with `blocked-by-policy`. The test then waits for a completion event while the server issues a second permission request. The focused test and three nextest retries failed identically.

This is a test-fixture defect, not a session-grant ownership defect: the JSONL transcript proves the thread persisted `api.example.com: allow` before the retry. The external DNS dependency makes the contract nondeterministic and conflicts with the proxy's deliberate DNS-rebinding protection.

## Scope

Replace the external request with a local one-shot HTTP listener on `127.0.0.1`, request and persist an explicit `127.0.0.1: allow` grant, then assert the listener was reached and the command completed. Keep the subsequent denied host request so the test continues to prove that the persisted session allowlist restricts later requests.

No runtime, CLI, TUI, JSONL schema, permission semantics, persistence format, or proxy policy changes are in scope.

## Behavior and Ownership

- The test-owned listener accepts one proxied request and returns a deterministic HTTP response; its nonblocking accept loop has a ten-second deadline so a missing retry cannot leave a detached fixture thread.
- The command/exec permission route remains the owner of the pending request, session metadata update, and retry.
- A session allow for the literal loopback host is explicit, so the proxy's existing loopback exception permits the test listener.
- The child server and listener are both joined before the test returns; no background fixture thread is detached.
- The existing later request to `blocked.orca.invalid` must emit a permission request because the session grant is scoped to `127.0.0.1`; denying that request must produce the existing terminal permission-denied error.

## Acceptance

1. The updated focused test fails with the previous external-DNS fixture in the current environment because the retry is blocked by policy.
2. With the local fixture, it receives one permission request for `127.0.0.1`, persists that exact session grant, observes the local response, and receives `command_exec_completed`.
3. `thread/read` and `thread/list` expose exactly the persisted loopback grant.
4. The later ungranted host emits a permission request and a deny response settles it with the existing command/exec permission-denied error.
5. Run the focused integration test, the relevant server tests, formatting, and diff hygiene after the change.

## Rollback

The slice is test-only. Reverting its single commit restores the prior fixture without affecting product behavior or persisted user data.
