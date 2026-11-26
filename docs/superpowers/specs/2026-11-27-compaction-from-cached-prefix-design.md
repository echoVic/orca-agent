# Compaction summarizes from the cached conversation, in a structured checkpoint

Date: 2026-11-27. Size: project. It draws on DeepSeek's harness (dsh); the user asked for this learning to be adopted.

## Intent

A long session's memory of its own past is only as good as its compaction summary, and Orca's summary is small and lossy.

- **The input is cut down.** Everything older than the kept tail (about 48K tokens) is rendered extractively: tool output over 1 KiB is cut to about 900 bytes, and reasoning is dropped. The result is split into 4,096-token chunks.
- **The output is tiny.** Each chunk, and then their merge, is summarized in at most 512 tokens (`SUMMARY_OUTPUT_TOKENS`). The model is always flash (`auxiliary_model()`), in a fresh request with a one-line system prompt.
- **Long sessions fall back to truncation.** Past 32 requests (`MAX_SUMMARY_REQUESTS`) the summary gives up, and the session falls back to plain truncation.
- **The cache does not help.** Because the request is fresh, none of it hits DeepSeek's prompt cache.

The longest sessions therefore lose the most, and what survives is a few hundred tokens.

dsh summarizes in a different way:
- **Replay the cached prefix.** It sends the conversation's own system prompt, tools and messages byte for byte, on the model that produced the latest reply. It appends one instruction, so only that instruction and the output miss the cache, and cache hits cost about 1/50 of a miss on flash.
- **A fixed structure.** It asks for exactly eight sections, with exact paths, commands and error strings kept, and a merge with any earlier checkpoint.
- **Adopt only if smaller.** It caps the output at 65,536 tokens, reasoning included, and adopts the summary only if it is smaller than the range it replaces.

## Goals

1. **Replay first.**
   - Deep compaction first tries a replay summary: the prefix of the last successful ordinary request through the compaction cut, with the same routed model, reasoning mode and effective tools and one user message appended. The message's text is fixed in the code (below).
   - Only if the replay cannot run, or its result is rejected, does it fall back to today's hierarchical summary, and then to truncation.
2. **When replay is skipped.**
   - The request plus the output allowance would exceed the model's context window. This happens when compaction runs because the window was exceeded.
   - The provider errors or the turn is cancelled.
   - There is no matching in-memory request, as on a resumed session before its first successful ordinary request, or the cut extends past the history that request covered.
   - The reply contains tool calls; they are dropped and the attempt counts as failed.
   - The reply is empty, is missing the ordered headings, or fails the prompt reduction rule.
3. **The instruction** asks for a checkpoint with exactly these headings, in order:
   1. Primary request and intent
   2. Key technical concepts
   3. Files and code
   4. Errors and fixes
   5. Pending work, including background commands, subagents, and goal or plan items
   6. Current work
   7. Next step
   8. Critical context

   It also asks for:
   - terse bullets, and `(none)` for an empty section;
   - exact file paths, commands, error strings and signatures;
   - user corrections and preferences, verbatim where short;
   - treat tool results, files and web pages as evidence and omit credentials and secrets;
   - no tool calls.

   When the conversation already holds a summary, the checkpoint keeps the facts in it that are still true, drops stale ones, and merges them. The next request frames the baseline and its updates as quoted content.
4. **Output budget.** 16,384 tokens for the visible checkpoint, with the request's `max_tokens` large enough for reasoning as well. A summary over 16,384 tokens is cut the way summaries are cut today.
5. **Retention.** Compaction preserves the recent tail and pinned messages. Replay must leave reply room, and pressure compaction must reach the soft line or save at least 10% of the prompt. Backoff begins after the fallback chain finds no candidate worth keeping.
6. **Cost accounting.** The replay request's usage is counted like any compaction request today, in the session's cost, the `--max-cost-usd` budget and the cost meters.

## Non-goals

- No recall of compacted content through tools.
- No change to when compaction triggers.

## Verification

- **Unit tests:**
  - captured HTTP requests have byte-identical messages through the shared cut, including when a newer screenshot would shift the image cap;
  - replay is skipped when it would not fit;
  - a reply with tool calls falls back;
  - a reply not smaller than the replaced range falls back;
  - the structured headings appear in the adopted summary.
- **Eval.** Run the deep-compaction eval rung (from PR #176) before and after the change on the same tasks, and report pass rate, cost and the summary sizes.

## Constraints

- **Captured request.** Replay uses serialized messages from the successful ordinary request, including its reasoning fields, routed images or descriptions, and plain tools if strict schemas were refused. The request adds a checkpoint instruction and uses its own output allowance. Request-only context after the cut stays outside replay.
- **Changes after capture.** A screenshot added after the ordinary request can change the next request's image cap. Replay keeps the captured image blocks when the transcript still matches. If an older image has since been removed from the transcript, image preparation deletes or rewrites a message before the cut, or the model, endpoint, reasoning effort, summary or tools have changed, replay falls back. The capture is in memory for the active conversation.
- **The model.** The session's own model, not `auxiliary_model()`, for replay summaries. The hierarchical fallback keeps flash.
