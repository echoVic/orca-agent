# ADR-0008: TUI State Owner Convergence

## Status

Accepted in v0.4.5; supersedes the v0.4.4 compatibility-facade wording.

## Context

The TUI had already separated rendering, input routing, interaction acknowledgement, and runtime projection, but `types.rs` still mixed protocol values, event reduction, transcript internals, interaction projections, and viewport bookkeeping. A single file made ownership implicit, forced unrelated features through one review surface, and pushed most tests toward an integration-only boundary.

## Decision

Use `AppState` as a composition root and give each independently invariant domain one owner:

| Owner | Responsibility |
| --- | --- |
| `protocol.rs` | `TuiEvent`, `UserAction`, interaction keys/responses, lifecycle and attachment values |
| `state_reducer.rs` | `AppState::update` and event dispatch/reducer helpers |
| `transcript_state.rs` | messages, revisions, render caches, search, stream assembly, finalization and flush watermarks |
| `interaction_state.rs` | pending user-input/MCP projections and staged acknowledgement payloads |
| `viewport_state.rs` | scroll/follow state, selection, frame geometry, clipboard feedback, and unread counts |

The aggregate owns composition and cross-owner transitions. Definitions are not duplicated, and `types` is not a compatibility facade: code imports every protocol or owner type from its owning module. An architecture contract rejects imports of dedicated owner types through `types`. Tests that prove one owner invariant live with that owner; only behavior spanning multiple owners remains in the state integration suite.

## Consequences

- A feature adding protocol, reduction, transcript, interaction, or viewport behavior has a bounded owner and test entry point.
- The compiler makes owner fields and protocol definitions explicit without introducing a second runtime state machine.
- Cross-owner integration tests remain necessary for lifecycle and projection ordering, but they are no longer the default home for local invariants.
- The source layout is intentionally a breaking internal refactor for contributors; runtime wire formats and persistence formats are unchanged.

## Verification

The v0.4.4 release gate covers the owner modules, the complete `orca-tui` library test suite, workspace checks, runtime/platform contract validators, npm staging, and the website build/SEO checks.
