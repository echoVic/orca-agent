# ADR 0007: Runtime-owned checkpointable agent continuation

## Status

Accepted and released in v0.3.25 on 2026-08-20.

## Context

Subagent and Workflow execution previously returned terminal text or cached
Workflow values, but did not expose one durable child-conversation identity.
Retrying after process loss could therefore restart exploration, lose tool
facts, or accidentally repeat an external side effect.

## Decision

The runtime owns a continuation lineage shared by synchronous subagents, async
subagent workers, and Workflow child agents. A lineage contains:

- UUIDv7 continuation, attempt, prompt, and checkpoint identities;
- a revision CAS plus owner and lease-epoch fence;
- a versioned child-conversation checkpoint with canonical digest;
- compatibility identity for agent type, model, delegation policy, effective
  working directory, worktree binding, MCP catalog, and external tool catalog;
- task and surface projections containing continuation, attempt, checkpoint,
  resumable, and indeterminate state.

Resume creates a new attempt and restores only durable conversation facts. It
generates the current system prompt, restores non-system messages, summaries,
tool terminals, bounded internal context, usage, and turn cursor, then appends
the new user prompt.

Before a tool enters dispatch, the child kernel persists a replay boundary from
its `ReplaySemantics`. Unknown external side effects become `Indeterminate`
until a later safe checkpoint contains an observed terminal result. A crash
with a safe checkpoint and no unsafe active boundary reconciles to `Suspended`;
otherwise it reconciles to `Indeterminate`.

Workflow resume order is completed cache, compatible safe continuation, fresh
attempt, then fail closed. Async workers renew task and continuation leases
under one owner and must commit continuation terminal state before task
terminal state. A retryable Workflow attempt keeps its recorded worktree while
the committed continuation remains resumable; other terminal outcomes finish
the worktree normally.

## Consequences

- Orca can continue the same child conversation after terminal follow-up or a
  cold restart without pretending to restore process-local execution state.
- Stale workers are fenced from checkpoint and terminal writes.
- Unknown side effects are visible and require inspection instead of automatic
  replay.
- Worktree continuations preserve the recorded path across safe retryable
  attempts, fail when that path no longer exists, and never reconstruct it.
- Existing callers that omit `resume_from` continue to create fresh child
  sessions.
