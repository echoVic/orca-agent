# A reply cut short is kept, not erased

Date: 2026-11-27. Size: small project.

## Intent

A reply can be cut short in two ways: the user presses Esc while the model is writing, or the reply hits the model's output limit. In both cases Orca erases what the user just read, and the model never sees it. The user sees the text vanish, and a follow-up such as "go on" or "expand your second point" fails, because the model does not know what it said.

DeepSeek's harness keeps the delivered part. On cancel it saves the delivered text and reasoning as an assistant message marked interrupted, and drops tool calls that never ran. At the output limit it keeps the text, drops every tool call, and ends the turn as limited.

## How it works today

- **Cancel.**
  - `runtime_actor/thread_actor_operation.rs` finalizes a cancelled generation by sending `AssistantPatch::StreamDiscarded { reason: GenerationCancelled }` for every open stream.
  - The TUI then deletes the streamed answer and reasoning (`TuiEvent::AssistantAttemptDiscarded` → `discard_current_assistant_attempt` in `crates/orca-tui/src/state_reducer.rs`).
  - The conversation is left without the reply (`provider_turn.rs`).
- **Output limit.**
  - `finish_reason=length` makes `deepseek_http.rs` return the error `Response truncated: model hit max_tokens limit (finish_reason=length); ask the model to continue in smaller chunks`, and the content is dropped.
  - The turn fails with `FailureClass::Provider`, and its streams are discarded with `ProviderFailed`.

## Goals

1. **On a user cancel, a reply with non-blank visible text is kept.**
   - The conversation records it as an assistant message holding the text and the reasoning streamed so far, with no tool calls, and the transcript records it so a resume keeps it.
   - The TUI keeps the text on screen and marks it with a dim `(interrupted)` line after it.
   - The turn still ends as `Cancelled`, exactly as today.
   - A cancel before any visible text (reasoning only, or nothing) behaves as today: discarded.
2. **At the output limit, the reply is kept the same way.**
   - Text and reasoning are kept, and every tool call is dropped.
   - The TUI marks the reply `(stopped at the output limit)`.
   - The turn still fails with the same message and failure class, so scripts and exit codes see no change. A resume, or a following "continue", has the partial reply in context.
3. **The model sees a kept reply as an ordinary earlier assistant message.**
   - No extra note is injected.
   - Reasoning stays attached as the request-building rules already require: an assistant message before the last user message needs no `reasoning_content`.
   - A kept reply must never be sent as the last message of a thinking-mode request without `reasoning_content`.
4. **ACP and other surface consumers stay consistent.**
   - Where the runtime surface needs a new patch or a new discard reason to say "kept", add it additively and update the runtime-surface contract and its pins.
   - ACP clients already received the chunks; they must not be told to drop them.

## Non-goals

- No automatic continuation after the output limit.
- No change to cancel semantics, exit codes, or the `Cancelled` and `Failed` terminals.

## Constraints

- **Runtime-surface contract.** Editing `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md` requires refreshing `private_contract_sha256` in its manifest and both sha256 entries in its `.digest.json`. Run `scripts/validate-runtime-surface-contract.mjs` and its test.
- **TUI golden frames.** If golden frames change, update them through the existing `ORCA_UPDATE_GOLDEN` path and review the diff.
- **Docs.** The TUI docs under `site/src/docs` (both languages), and `docs/harness-contract.md` if it describes a cancelled reply.
