# Provider errors say what happened; retries are visible

Date: 2026-11-27. Size: small project.

## Intent

When DeepSeek refuses or fails a request, users and scripts should learn three things:
- what kind of failure it was;
- that Orca is retrying, and how long it will wait;
- the request id DeepSeek gave, for support.

The context meter should also show the real size of the request, not the sum of every attempt.

## How it works today

- **Errors are classified by substring.** `classify_deepseek_error` (`crates/orca-provider/src/deepseek_http.rs`) looks for words in the error text: a `429` or `rate limit` anywhere is RateLimit, and any `500`/`502`/`503`/`504` anywhere is Server.
  - So an error body that merely mentions such a number is misread, and retried as a server failure.
  - 401, 403 and 402 all become `Other`. That stops retries, but the message does not say that the key was refused or the balance ran out.
- **No request id is kept** from the response headers.
- **Retries are silent.**
  - `provider_turn.rs` retries up to 5 times within 120 s (`RetryBudget::new(5, Duration::from_secs(120), …)`).
  - `commit_surface_provider_attempt_failure` (`runtime_actor/thread_actor_generation.rs`) receives the failure text and ignores it (`_message`).
  - The TUI shows nothing while it waits, which looks like a hang, and `orca exec` JSONL records no retry.
- **The context meter adds up attempts.** After a retry, `provider_turn.rs` merges the failed attempts' usage into `response.usage` (it should, for cost), then reports `context_updated(usage.input_tokens, …)` from that merged value. So the meter shows the prompt counted once per attempt.

## Goals

1. **Classify by HTTP status first.**
   - When the request got an HTTP status, it decides the kind:
     - 429 → `RateLimit`;
     - 500–599 → `Server`;
     - 400 or 413 whose body is a context-length error → `ContextExceeded`;
     - every other 4xx → `Other`, which is not retried.
   - Only failures without a status (transport, stream) use the text rules, and those no longer match bare numbers.
   - No `ProviderErrorKind` variant is added: the kind is serialized, and older versions must keep reading what this one writes.
2. **Keep the status and request id.**
   - `ProviderError` gains `http_status: Option<u16>` and `request_id: Option<String>`. Both are `#[serde(default, skip_serializing_if = "Option::is_none")]`, so the JSON shape only grows.
   - The request id is read from the first present header of `x-request-id`, `request-id` or `x-deepseek-request-id`.
   - When either is known, the final error message ends with `(HTTP <status>, request id <id>)`, showing whichever are known.
3. **Plain messages for the common refusals.**
   - 401 and 403 begin `DeepSeek rejected the API key`.
   - 402, or a body mentioning insufficient balance, begins `DeepSeek reports an insufficient account balance`.
   - Each is followed by the provider's own text.
4. **Visible retries.**
   - Before each backoff the runtime reports a retry with: attempt number, maximum attempts, delay in milliseconds, kind, and the short failure text.
   - The TUI shows it on the status line as `Retrying in <n>s (attempt <k> of <max>): <reason>` until the next attempt starts.
   - `orca exec --output-format jsonl` emits it as an event, added under the contract's rule that consumers ignore event types they do not know. Use the existing event-naming style; `provider.retry` is suggested.
   - It reaches ACP as a status update if ACP has a matching notion; otherwise ACP is untouched.
5. **The context meter shows one request.** `context_updated` uses the successful attempt's own `input_tokens` (`attempt_usage`). Cost and usage totals keep the merged value.

## Non-goals

- No new failure class or exit code, and no change to which kinds are retried, apart from the misreads Goal 1 removes.
- No change to the retry budget.

## Constraints

- **Contract docs.** A new JSONL event and its fields go into `docs/harness-contract.md` and into the JSONL docs in `site/src/docs`, in both English and Chinese, wherever events are listed.
- **Runtime-surface contract.** If a surface type changes, update the contract document and its pinned hashes exactly as `scripts/validate-runtime-surface-contract.mjs` reports. Editing `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md` also requires refreshing `private_contract_sha256` in its manifest and both sha256 entries in its `.digest.json`.
