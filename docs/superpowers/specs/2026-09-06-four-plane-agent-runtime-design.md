# Four-Plane Agent Runtime Design

## Goal

Make every delegated agent an independent runtime thread with its own transcript,
execution authority, and durable event stream. The root conversation receives only
one delegation announcement; the Agent Dock is the live cross-thread projection.

## Architecture

The runtime owns four explicit planes:

1. **Threads**: one root thread and one child thread per delegated agent. Child
   transcripts never enter the root transcript.
2. **Controller and registry**: `AgentController` creates children and
   `AgentRegistry` reduces one typed `AgentEventEnvelope` journal into dock
   summaries. The registry is the only agent-state presentation source.
3. **Presentation**: `AppState` stores root/child focus and dock selection. The
   conversation renderer shows the root or focused child transcript; the dock is
   present in both views. `/agents` remains a detail view, not a prerequisite.
4. **Execution broker**: `ApprovalPolicy` and `ExecutionProfile` are independent.
   A missing local sandbox produces one typed availability decision instead of
   rejecting every child or repeating an error per tick.

## Event Contract

`AgentEventEnvelope` is the canonical agent log. Every event carries schema
version, event id, root/agent/thread/attempt identities, source sequence, timestamp,
and a SHA-256 payload digest. `AgentRegistry` applies events exactly once and
dead-letters malformed records while advancing the poisoned sequence, so a bad
frame cannot be replayed indefinitely.

The root surface may project one `DelegationStarted` system message, but it does
not project child activity or output. Child activity is read from the focused
child thread surface and dock summaries come from the registry projection.

## Interaction

- `Shift+Down` / `Shift+Up`: cycle Main and active child rows.
- `Enter`: focus the selected child and show its independent transcript.
- `Esc`: return to Main before closing any management panel.
- Child input and interaction responses are routed to the focused thread only.

## Execution and failure

`ApprovalPolicy` (`Ask`, `Auto`, `Never`) is evaluated separately from
`ExecutionProfile` (`ReadOnly`, `Workspace`, `TrustedHost`, `RemoteSandbox`). A
child can only narrow its parent's profile. `Workspace` on an unavailable local
backend becomes an explicit `SandboxUnavailable` decision with one user-actionable
choice; no child or actor tick repeats the same diagnostic.

Relay delivery is at-least-once, but the registry journal has one cursor per
attempt and dead-letter quarantine for invalid frames. Terminal state is sticky,
and late activity cannot reopen a completed child.

## Migration boundary

Backward compatibility is intentionally out of scope. `ChatMessage::Subagent`,
`WorkflowTasksUpdated`, and `TaskStatusUpdated` are removed as agent presentation
authorities after the registry projection is wired. Existing derived state is
rebuilt from the new event journal; source transcripts are preserved.

## Verification

The implementation must pass focused unit tests for event reduction, dock focus,
profile inheritance, sandbox-unavailable handling, and dead-letter cursor advance;
the locked workspace gate; a real PTY/CUA run showing Main plus active children,
focus/return, and one launch announcement; site/docs validation; and release/npm
publication verification before Rust build caches are removed.
