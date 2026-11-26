# Compaction From the Cached Prefix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deep compaction first asks the session's own model for a structured checkpoint. It replays the conversation as already sent, byte for byte up to the compaction cut, with one instruction appended, so the replayed part hits DeepSeek's prompt cache. Today's hierarchical summary and then truncation stay as fallbacks.

**Architecture:** A new `replay_summary` path sits in front of today's hierarchical summary inside `summarize_collapsed_messages` (`crates/orca-provider/src/context.rs`). It builds the API messages with the same function the next request would use (`conversation_to_api_messages`, `crates/orca-provider/src/deepseek_http.rs`), applied to the conversation cut at the end of the collapsed range. It then appends the instruction as a final user message after serialization, and sends it with the session's model, reasoning settings and tool list.

**Tech Stack:** Rust (orca-provider `context.rs`, `deepseek_http.rs`, `summary_cache.rs`); the eval harness in `scripts/eval/capability/`.

**Spec:** `docs/superpowers/specs/2026-11-27-compaction-from-cached-prefix-design.md`

## Global Constraints

- **Byte-identical prefix.** The replayed messages up to the cut are byte-identical to how the next ordinary request would serialize them, `reasoning_content` included. The tool list is the session's, sorted as today.
- **Model and fallbacks.** The replay uses the session's model and reasoning effort, never `auxiliary_model()`. The hierarchical fallback keeps flash and today's limits.
- **Unchanged behaviour:**
  - `CompactionKind::RemoteSummary` is still the kind reported;
  - pinned messages, the kept tail, the 10% minimum gain and backoff, and when compaction triggers do not change;
  - cost and usage of the replay request are recorded through `CompactionObservation`, like today's summary requests.
- Commit after each task; never push. No `TMPDIR`/`HOME` override; never touch `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.

## Review Focus

- **A conversation whose collapsed range ends inside a tool-call unit.** Partitioning already cuts only between units; the replay must cut at the same place. Task 1 test `replay_cuts_where_the_partition_cuts`.
- **Thinking mode with tool-loop assistant messages before the cut.** Their `reasoning_content` is serialized exactly as in the next request. Task 1 test `replay_keeps_reasoning_content_as_the_next_request_would`.
- **The model replies with tool calls, reasoning only, or a checkpoint larger than the range it replaces.** All three fall back to the hierarchical summary. Task 2 tests.
- **The request would not fit in the model's window plus the output allowance.** It falls back without sending. Task 2 test `replay_is_skipped_when_it_would_not_fit`.
- **A conversation that already holds a summary baseline and deltas.** The replay includes them in place, as the next request would, and the adopted checkpoint replaces baseline and deltas. Task 2 test `a_replay_checkpoint_replaces_the_previous_summary`.

---

### Task 1: Build the replay request

**Files:**
- Modify: `crates/orca-provider/src/context.rs`. Add the replay builder near `summarize_collapsed_messages`.
- Modify: `crates/orca-provider/src/deepseek_http.rs`, only if `conversation_to_api_messages` needs a small entry point to serialize a cut conversation with no internal-context overlay.

**Interfaces:**
- Produces: `fn replay_summary_messages(conversation: &Conversation, cut: usize) -> Vec<ApiMessage>`.
  - It serializes `conversation.messages[..cut]` with the summary state exactly as `conversation_to_api_messages` would for the next request. The internal-context overlay is omitted, because it sat after the cut in the earlier request.
  - It then pushes one `user` message whose content is `REPLAY_SUMMARY_INSTRUCTION`.
- Produces: `const REPLAY_SUMMARY_INSTRUCTION: &str`, with exactly this text:

```text
Write a checkpoint of the conversation above so that the work can continue from it after the earlier messages are removed. Use exactly these sections, in this order, as Markdown headings:

## Primary request and intent
## Key technical concepts
## Files and code
## Errors and fixes
## Pending work
## Current work
## Next step
## Critical context

Rules:
- Use terse bullets. Write "(none)" under a section with nothing to say.
- Keep exact file paths, commands, error messages, identifiers and signatures.
- Keep the user's corrections and preferences, verbatim when short.
- Under Pending work, list background commands, subagents, and plan or goal items that are not finished.
- If the conversation contains an earlier summary, keep what is still true in it, drop what is no longer true, and merge the rest into this checkpoint.
- Do not call any tools. Reply with the checkpoint only.
```

- [ ] **Step 1: Write the failing tests:**
  - `replay_messages_equal_the_next_request_up_to_the_cut`: for a conversation of system, user, assistant with a tool call, tool, assistant and user messages, and a cut after the first tool unit, the serialized JSON of `replay_summary_messages(conv, cut)[..n]` equals the first n messages of `conversation_to_api_messages(conv)`, where n is the number of API messages produced from `messages[..cut]` plus the injected summaries. The last element is the user instruction.
  - `replay_keeps_reasoning_content_as_the_next_request_would`: thinking-mode messages after the last user message, before the cut, carry the same `reasoning_content` in both.
  - `replay_cuts_where_the_partition_cuts`: the cut used equals the end of `partition_for_compaction`'s collapsed range.
  - `replay_instruction_names_the_eight_sections`.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-provider --lib replay -- --test-threads=1`
Expected: FAIL, because the functions are missing.

- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them to verify they pass.** Use the same command. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/orca-provider/src
git commit -m "feat(compaction): build a summary request that replays the cached conversation"
```

### Task 2: Use the replay first, and fall back as today

**Files:**
- Modify: `crates/orca-provider/src/context.rs` (`summarize_collapsed_messages` and its helpers) and `summary_cache.rs`, if the baseline/delta state needs a "replace with checkpoint" operation.
- Test: the `context.rs` tests module, using the fake-provider helpers the existing summary tests use.

**Interfaces:**
- Consumes: Task 1.
- Produces: `fn replay_summary(provider_kind, conversation, partition, context_config, provider_config, cancel, observation) -> Option<String>`.
  - It returns the adopted checkpoint text, or `None` to fall back.
  - It sends with the session's model and reasoning effort, the session's sorted tools, and `max_tokens` = 32,768.
  - Visible text past 16,384 tokens is cut with the same `bounded_summary` the hierarchical path uses.

**Rules for `None`:**
- The estimated request tokens plus 32,768 exceed the model's context window.
- The request is cancelled or errors.
- The reply has tool calls.
- The reply's visible text is empty.
- The checkpoint's token count is not smaller than the collapsed range's.

- [ ] **Step 1: Write the failing tests:**
  - `deep_compaction_uses_the_replay_checkpoint`: a fake provider returns a checkpoint with the eight headings. The compacted conversation's summary is that text, the kind is `RemoteSummary`, the request went to the session's model, and exactly one summary request was made.
  - `replay_is_skipped_when_it_would_not_fit`: no replay request is sent, and the hierarchical path runs.
  - `a_reply_with_tool_calls_falls_back`.
  - `an_empty_or_reasoning_only_reply_falls_back`.
  - `a_checkpoint_not_smaller_than_the_range_falls_back`.
  - `a_replay_checkpoint_replaces_the_previous_summary`.
  - `replay_usage_is_recorded_in_the_observation`.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-provider --lib compaction -- --test-threads=1`
Expected: the new tests FAIL.

- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them to verify they pass.**

Run: `cargo test -p orca-provider --lib -- --test-threads=1`, then `cargo test -p orca-runtime --lib compaction -- --test-threads=1`.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-provider/src
git commit -m "feat(compaction): deep compaction asks the session's model for a structured checkpoint first"
```

### Task 3: Measure and document

**Files:**
- Modify: `docs/` and the site docs that describe compaction (`site/src/docs/md/en` and `zh`; search them for "compaction").

- [ ] **Step 1: Run the deep-compaction rung if a DeepSeek key is configured.**
  - Check for `DEEPSEEK_API_KEY` in the environment, or Orca's configured key, without printing it.
  - If one is present, run `C-real-deep-compaction` with the capability runner described in `scripts/eval/capability/README.md`, once with a binary built from `origin/main` and once with this branch.
  - Report pass or fail, cost, summary request count, and summary size for each.
  - Delete the attempt workspaces afterwards; they trip the Windows platform validator.
  - If no key is present, say so, and skip this step.
- [ ] **Step 2: Update the docs.** Deep compaction now asks the session's model for a structured checkpoint, and the replayed conversation is billed mostly at cache-hit prices. The fallbacks are unchanged.
- [ ] **Step 3: Verify**

Run:

```bash
cargo fmt --all -- --check
cargo clippy -p orca-provider -p orca-runtime --all-targets --locked
env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs
env -u NODE_OPTIONS npm --prefix site ci && env -u NODE_OPTIONS npm --prefix site run build
```

Expected: every command exits 0.

- [ ] **Step 4: Commit**

```bash
git add docs site/src
git commit -m "docs(compaction): deep compaction writes a structured checkpoint from the cached conversation"
```
