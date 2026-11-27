# Keep Interrupted Replies Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the user cancels a reply that already shows text, or a reply hits the output limit, the delivered text and reasoning stay on screen and in the conversation, marked as cut short. The turn's terminal stays as it is today.

**Architecture:** The provider returns the partial content together with the length error. The runtime commits a kept reply to the conversation and transcript before it finalizes the generation, and marks its streams kept instead of discarded. The TUI keeps kept streams and appends the marker line.

**Tech Stack:** Rust: orca-provider `deepseek_http.rs`; orca-runtime `provider_turn.rs`, `runtime_actor/thread_actor_operation.rs` and the surface types; orca-tui `surface_projection.rs` and `state_reducer.rs`; ACP observer and agent.

**Spec:** `docs/superpowers/specs/2026-11-27-keep-interrupted-replies-design.md`

## Global Constraints

- `Cancelled` and `Failed` terminals, their messages, failure classes and exit codes are unchanged.
- Only replies with non-blank visible text are kept, and tool calls are never kept.
- Surface and contract changes are additive; update the pins exactly as the validator reports.
- Commit after each task; never push. No `TMPDIR`/`HOME` override; never touch `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.

## Review Focus

- **A cancel during the tool-call part of a reply, after visible text.** The text is kept and the half-written tool call is not; no tool runs. Task 2 test `a_cancel_mid_tool_call_keeps_only_the_text`.
- **A cancel with reasoning only.** Discarded, as today. Task 2 test `a_cancel_with_reasoning_only_is_discarded`.
- **A kept reply followed by the next user message in thinking mode.** The next request is accepted by the request builder's rules. Assistant messages after the last user message carry `reasoning_content`, and the kept one precedes the user message. Task 3 test `the_request_after_a_kept_reply_is_valid_in_thinking_mode`.
- **Resume after a kept reply.** The reply and its marker are rebuilt on screen from the transcript. Task 3 test `a_kept_reply_survives_resume`.
- **Two quick Esc presses.** A second cancel while finalizing keeps the reply once, never twice. Task 2 test `a_double_cancel_keeps_the_reply_once`.

---

### Task 1: The provider returns the partial reply with the length error

**Files:**
- Modify: `crates/orca-provider/src/deepseek_http.rs`. `DeepSeekRequestError` (or the type the length branch returns) carries the assembled visible content and reasoning, with no tool calls. The error message is unchanged.

**Interfaces:**
- Produces: `partial_reply: Option<PartialReply>` on the provider error that reaches the runtime, where `PartialReply { content: String, reasoning: Option<String> }`. It is set only for `finish_reason=length` when `content.trim()` is non-empty.

- [ ] **Step 1: Write the failing test** `a_length_finish_returns_the_partial_reply`.
  - Use the module's fake stream helpers to stream visible text and a partial tool call, then `finish_reason=length`.
  - The error message is the same text as today.
  - `partial_reply.content` is the streamed text, with no tool call.
  - The bounded-summary truncation path is unaffected.
- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p orca-provider --lib a_length_finish_returns_the_partial_reply`
Expected: FAIL.

- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run it to verify it passes.** Use the same command, plus `cargo test -p orca-provider --lib -- --test-threads=1`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/orca-provider/src
git commit -m "fix(provider): a reply that reaches the output limit comes back with the error"
```

### Task 2: The runtime keeps a cut-short reply

**Files:**
- Modify: `crates/orca-runtime/src/provider_turn.rs` and `runtime_actor/thread_actor_operation.rs`, plus the surface types where streams are finalized. Commit the kept reply, then mark its streams kept, with either a new discard reason or a new patch.
- Modify: the runtime-surface contract doc, its manifest and its digest, if a surface type changes.

**Interfaces:**
- Consumes: `PartialReply` from Task 1.
- Produces: a kept reply is an `assistant` conversation message that carries a marker, `cut_short: Some(CutShort::Interrupted | CutShort::OutputLimit)`, or an equivalent field on the message or the transcript record.
  - It is persisted, and serialized with `skip_serializing_if = "Option::is_none"` so ordinary messages are unchanged on disk.
  - The surface tells consumers it was kept, with the same reason.

- [ ] **Step 1: Write the failing tests**, using the runtime's scripted-provider test helpers:
  - `a_cancelled_reply_with_text_is_kept`: cancel after the text deltas. The conversation ends with an assistant message holding that text, marked `Interrupted`. The terminal is `Cancelled`.
  - `a_cancel_mid_tool_call_keeps_only_the_text`.
  - `a_cancel_with_reasoning_only_is_discarded`.
  - `a_double_cancel_keeps_the_reply_once`.
  - `a_reply_at_the_output_limit_is_kept_and_the_turn_still_fails`: a length finish. The assistant message is kept and marked `OutputLimit`, and the terminal is `Failed` with today's message.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-runtime --lib kept -- --test-threads=1`. Name the tests so that the filter matches all five.
Expected: FAIL.

- [ ] **Step 3: Implement.** Update the surface contract docs and pins.
- [ ] **Step 4: Run them to verify they pass.** Use the same command, then:

```bash
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A crates docs
git commit -m "fix(runtime): a reply cut short by a cancel or the output limit is kept"
```

### Task 3: The TUI and ACP show a kept reply; resume and the next request work

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs` and `state_reducer.rs`. A kept stream is not discarded; a dim marker line follows it. Also the transcript rebuild on resume.
- Modify: the ACP observer and agent, so a kept stream sends no retraction.
- Modify: the TUI docs under `site/src/docs/md/en` and `zh`, where Esc or interruption is described.

- [ ] **Step 1: Write the failing tests:**
  - `an_interrupted_reply_stays_on_screen`: a TUI reducer or projection test. Message deltas, then a kept (`Interrupted`) event. The text remains, followed by a line `(interrupted)`.
  - `a_reply_stopped_at_the_output_limit_stays_on_screen`, with the line `(stopped at the output limit)`.
  - `a_kept_reply_survives_resume`.
  - `the_request_after_a_kept_reply_is_valid_in_thinking_mode`: an orca-provider request-building test. A conversation with a kept reply followed by a user message builds messages that the existing thinking-mode validation accepts.
  - ACP: `a_kept_stream_is_not_retracted`, following the observer's existing tests.
- [ ] **Step 2: Run them to verify they fail**

Run each test by name with `cargo test -p <crate> --lib <name>`.
Expected: FAIL.

- [ ] **Step 3: Implement.** Update the golden frames through `ORCA_UPDATE_GOLDEN` only if they change, and review the diff.
- [ ] **Step 4: Verify**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 4
env -u NODE_OPTIONS cargo test -p orca-tui --lib -- --test-threads=1
env -u NODE_OPTIONS npm --prefix site ci
env -u NODE_OPTIONS npm --prefix site run build
```

Expected: every command exits 0, and clippy prints no warnings.

- [ ] **Step 5: Commit**

```bash
git add -A crates docs site/src
git commit -m "fix(tui): an interrupted reply stays on screen, marked as cut short"
```
