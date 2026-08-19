# Pending Store Deletion Gate

Updated: 2026-08-18

## Result

The deletion gate is complete. The Durable Interaction Broker now owns durable
interaction routing, continuation, and cold recovery across every supported
interaction and surface. The `RuntimePendingInteractionStore` compatibility
shim, its source file, and its crate export have been deleted.

## Gate Evidence

| Requirement | Result | Evidence |
|---|---|---|
| Interaction coverage | Pass | Tool Approval, Permission Request, User Input, and MCP Elicitation all use the durable broker. |
| Surface coverage | Pass | The TUI, ACP, JSONL, and Headless 4×4 interaction matrix passes. |
| Exact routing | Pass | Stable route epoch, response token, grant, and operation fence selectors are exact and fail closed. |
| Tool Approval continuation | Pass | An `InvocationStarted` receipt prevents an approved invocation from being ambiguously replayed. |
| Permission continuation | Pass | Permission recovery permits retry only before the protected side effect starts. |
| User Input and MCP continuation | Pass | After an answer is accepted, each path resumes through a stable durable continuation operation. |
| Cold recovery | Pass | Missing, executing, unsafe, unsupported, and stale-context recovery states durably fail closed. |
| Shim deletion | Pass | `crates/orca-runtime/src/runtime_pending_interaction.rs` and the `lib.rs` export are deleted; production and test source contain zero old store or builder symbols. |
| Validation | Pass | The runtime-surface validator, runtime all-targets checks, and TUI checks pass. |

## Breaking Rust API Impact

This deletion is intentionally source-breaking for downstream Rust callers that
still compile against the compatibility layer:

- `orca_runtime::runtime_pending_interaction` is no longer exported.
- `RuntimePendingInteractionStore` and the other public types formerly exposed
  by that module are no longer available.
- `HostedTurnRequest::with_pending_interactions` is removed rather than retained
  as a no-op builder.

Downstream callers must use the typed runtime-surface interaction and operation
contracts. There is no remaining process-local pending-interaction store or
compatibility builder to preserve the old API shape.

## Verification

The completed gate is backed by the repository runtime-surface validator and
its self-tests, the locked runtime all-targets check, the TUI checks, and a
source search confirming that the deleted store and builder symbols have no
production or test references.
