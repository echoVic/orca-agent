# Automatic Network Permission Requests for Active Thread Profiles

## Problem and evidence

`command/exec` can run with a network allowlist from either an explicit
`permissionProfile` request or an active permission profile inherited from a
thread. The server creates a `RuntimeNetworkProxy` for both paths, but it only
keeps the block-report receiver when the request includes an explicit
`permissionProfile`. An inherited profile can therefore report a blocked host
in the proxy response, yet has no route to the owning thread permission
request. The command completes with proxy diagnostics instead of pausing for a
grant and retrying.

This is a boundary defect: the network policy is runtime-owned, while the
permission request route is conditionally owned by the request parser instead
of by the effective policy.

## User value

TUI/server callers that establish a thread permission profile get the same
allowlist permission interaction as callers that repeat the profile on every
command. A blocked command pauses with an observable permission request,
resumes exactly once after an allow, and remains a final denial when the
profile denylist blocks it.

## Scope

In scope:

- retain a bounded network-block report route whenever the effective sandbox
  has requestable network policy;
- preserve the existing JSONL permission route, request id, session/turn grant
  semantics, retry behavior, and denylist errors;
- add a server behavior test for an active thread profile that requests a
  network grant and retries the same command.

Out of scope:

- changing permission response wire shapes or persisted thread metadata;
- changing denylist policy, proxy behavior, or non-network filesystem prompts;
- redesigning long-lived server connection scheduling.

## Failure semantics and ownership

The server owns the `CommandExecProcess` and its report receiver. The proxy
owns report production. The existing `permission_routes` registry owns the
pending request and consumes one response; an allow response retries the
original command with the returned network overlay. A deny response produces a
terminal denial. If no thread id exists, there is no permission route and the
existing final command result remains unchanged. Proxy report queues stay
bounded and nonblocking.

## Acceptance

1. A thread with an active allowlist profile, followed by `command/exec` that
   omits `permissionProfile`, emits one `permission_request` for the blocked
   host before `command_exec_completed`.
2. Allowing that request with session scope retries the command once, emits the
   successful completion, and persists the network grant on the thread.
3. A denylisted host still emits the existing policy-denial error and never
   emits a permission request.
4. Existing explicit-profile and non-network command tests remain green.

## Verification

- RED/GREEN focused server tests for the inherited-profile flow;
- `cargo test -p orca-runtime server:: --lib -- --test-threads=1`;
- `cargo test -p orca-runtime --lib -- --test-threads=1`;
- `cargo fmt --all -- --check` and `git diff --check`.

## Migration and rollback

No protocol or persistence migration is required. The change is isolated to
the effective-policy setup in `run_command_exec`; reverting the commit restores
the prior conditional reporter behavior without altering stored records.
