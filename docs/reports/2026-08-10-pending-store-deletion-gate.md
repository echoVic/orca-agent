# Pending Store Deletion Gate

## Result

The deletion gate is not passed on `dcf95aac2`. The process-local store is no
longer a runtime owner, but removing its public compatibility API now would
break the still-active legacy `HostedTurnRequest`/Goal continuation path.

## Evidence

| Requirement | Result | Evidence |
|---|---|---|
| Runtime host no longer reads the map | Pass | `HostedTurnRequest::with_pending_interactions` is a documented no-op; the legacy Goal preflight has no store read. |
| Server/CLI production callers no longer compile against the store | Pass | Repository search finds no server/CLI production import; remaining callers are the compatibility regression and TUI-only MCP mode enum. |
| Durable broker recovery exists | Pass | `runtime_surface_interaction` cold-recovery fixtures rematerialize provider tools and fail closed when a live-only waiter is unavailable. |
| Legacy Goal path is gone | **Blocked** | `RuntimeHost::dispatch_legacy_goal_continuation` and its `HostedTurnRequest` workers remain in production, and TUI/ACP/controller still construct `HostedTurnRequest`. |
| Rust API compatibility gate | **Blocked** | `cargo-semver-checks` is not installed in the current environment; no public symbol removal was attempted. |

## Verification Commands

The following commands are the executable gate for the next migration window:

```bash
cargo test -p orca-runtime --test runtime_host legacy_goal_pending_store_does_not_block_continuation -- --exact --nocapture
cargo test -p orca-runtime --test runtime_surface_interaction cold_recovery_rematerializes_provider_tool_before_cancelling_unavailable_approval -- --exact --nocapture
cargo test -p orca-runtime --test runtime_surface_interaction cold_recovery_rematerializes_provider_tool_before_cancelling_unavailable_permission -- --exact --nocapture
cargo check -p orca-runtime -p orca-tui --all-targets --locked
rg -n "RuntimePendingInteractionStore|with_pending_interactions" crates/orca-runtime/src crates/orca-tui/src
cargo-semver-checks check-release -p orca-runtime --release-type major
```

The first four commands provide the current behavioral evidence. The search is
expected to continue finding the compatibility builder and the legacy Goal
worker until the migration slice removes them. The final command must run only
after those production paths are gone and a major-version API migration has
been chosen.

## Deletion Conditions

The shim may be deleted only after `HostedTurnRequest` no longer owns or
dispatches a legacy Goal continuation, TUI/ACP/controller callers use the typed
surface operation request, and the Rust API gate runs against the published
baseline. Until then, retain the no-op builder and public store types; they are
compatibility projections, not a second runtime fact source.
