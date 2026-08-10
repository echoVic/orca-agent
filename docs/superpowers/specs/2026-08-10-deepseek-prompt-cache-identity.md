# DeepSeek Prompt Cache Identity

## Problem And Evidence

DeepSeek usage already reports `prompt_cache_hit_tokens`, but blade-deepseek
does not define which request prefix is expected to remain cache-compatible.
`deepseek_http::conversation_to_api_messages` is the actual message lowering
boundary, while `tool_schema` currently preserves caller insertion order. Two
semantically identical tool sets can therefore produce different request
prefixes, and session history contains no durable evidence of the exact prefix
sent for a turn.

This is a boundary defect: provider wire lowering is authoritative, but runtime
history and fork behavior have no typed cache-identity contract.

## User Value

Stable DeepSeek request prefixes can reuse provider-side prompt cache entries,
reducing input cost and latency in long TUI sessions. A durable checkpoint also
makes a cache miss explainable without storing prompt contents or creating a
second conversation state.

## Scope

- Canonicalize advertised DeepSeek tools before they enter the request body.
- Derive a versioned, content-free checkpoint from the actual lowered message
  prefix, selected tool payload, model, endpoint, and reasoning configuration.
- Persist one typed checkpoint immediately before each real DeepSeek provider
  request when session history is enabled.
- Keep checkpoints as audit data. Conversation messages remain the only source
  used to resume or fork a session.
- Add a credential-gated real DeepSeek verifier that sends two requests sharing
  a substantial prefix and requires a non-zero cache hit on the second request.

## Non-Goals

- No local response cache and no promise that DeepSeek will retain a remote
  cache entry for a particular duration.
- No mutation of system prompts, summaries, or conversation ordering outside
  the existing wire-lowering path.
- No checkpoint inheritance across forks and no cache checkpoint replay during
  resume.
- No change to CLI, TUI, server/JSONL, or runtime-surface protocols.
- No attempt to hide a beta strict-schema fallback: the checkpoint identifies
  the primary request payload selected before the request starts.

## Identity Contract

`PromptCacheCheckpoint` contains only hashes and counts:

- a schema version;
- a scope digest covering the normalized endpoint, model, and reasoning mode;
- a message-prefix digest and lowered message count;
- a tool-payload digest and advertised tool count.

Hashes are SHA-256 over domain-separated JSON bytes. No prompt, tool arguments,
API key, or user content is persisted in the checkpoint. A later request extends
a checkpoint only when its scope and tools are identical and the digest of its
first `message_count` lowered messages matches the stored prefix digest.

Tool definitions are sorted by tool name before both strict and non-strict
lowering. JSON object keys remain canonical through `serde_json::Value`; the
existing DeepSeek limit is applied after sorting so insertion order cannot
change which 128 tools are advertised.

## Ownership And Lifecycle

- `orca-provider` owns wire lowering and checkpoint calculation.
- `SessionWriter` owns durable append ordering.
- `RuntimeProviderTurnStep` appends the checkpoint after pre-model hooks,
  steering, and runtime system messages have produced the final model
  conversation, and before `start_streaming` owns a provider worker.
- Cancellation before this boundary writes no checkpoint. Cancellation,
  disconnect, provider error, or retry after the append leaves an audit record
  of the attempted request and does not alter conversation recovery.
- Reactive compaction creates a new provider attempt and therefore a new
  checkpoint. Restarts read but ignore checkpoints for execution.

## Persistence And Compatibility

The JSONL history gains the additive `provider.prompt_cache_checkpoint` record.
Current readers explicitly ignore it when reconstructing transcripts, usage,
plans, and completion state. Fork creation continues to write only the selected
conversation and summary state into the child session, so parent checkpoints
cannot become child state. The record contains no secret material and the
redaction path treats it as already content-free.

## Acceptance

1. Permuting otherwise identical tool definitions produces byte-identical
   strict and non-strict DeepSeek tool payloads.
2. A checkpoint accepts an extended conversation with the same lowered prefix,
   scope, and tools, and rejects a changed system message or changed tools.
3. A runtime DeepSeek turn writes the typed checkpoint before provider dispatch;
   mock and fixture providers do not write it.
4. Transcript recovery ignores checkpoints, and a fork does not copy them.
5. The focused provider and runtime tests, provider/runtime all-target checks,
   formatting, and `git diff --check` pass.
6. When credentials are configured, two real DeepSeek requests with a long
   shared prefix complete and the second reports `cache_tokens > 0`. Otherwise
   the verifier exits successfully with an explicit credential-gated skip.

## Migration And Rollback

The change is additive except for deterministic tool ordering. It requires no
data migration. Reverting the slice restores caller tool order and stops writing
new checkpoint records; existing records remain inert audit entries for current
readers.
