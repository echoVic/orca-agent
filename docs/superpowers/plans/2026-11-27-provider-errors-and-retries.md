# Provider Errors and Retries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Provider failures are classified by HTTP status, carry the status and request id, and say "key rejected" or "insufficient balance" plainly. Every retry is shown in the TUI and in `orca exec` JSONL, and the context meter reports one request's size.

**Architecture:** orca-provider records the status and request id on `ProviderError` at the HTTP layer and classifies by status. orca-runtime reports each retry through the provider-attempt path that already exists, extended with the attempt and delay. The TUI and the JSONL writer render the retry.

**Tech Stack:** Rust (orca-core `provider_types.rs`, orca-provider `deepseek_http.rs`/`http_client.rs`, orca-runtime `provider_turn.rs` and `runtime_actor/thread_actor_generation.rs`, orca-tui), docs.

**Spec:** `docs/superpowers/specs/2026-11-27-provider-errors-and-retries-design.md`

## Global Constraints

- No new `ProviderErrorKind` variant. The new `ProviderError` fields are optional and skipped when `None`.
- The retry budget (5 attempts, 120 s) and the set of retryable kinds stay as they are.
- Contract changes are additive and documented. Update runtime-surface contract pins exactly as the validator reports.
- Commit after each task; never push. No `TMPDIR`/`HOME` override; never touch `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.

## Review Focus

- **A 400 whose body mentions "5000 tokens".** It is `Other` and not retried, rather than `Server` and retried. Task 1 test `a_client_error_mentioning_500_is_not_a_server_error`.
- **A response with two request-id headers.** The first in the listed order wins. Task 1 test `request_id_prefers_x_request_id`.
- **A retry announced, then the user cancels during the backoff.** The status line clears, and no attempt-started line follows. Task 2 test `a_cancel_during_backoff_clears_the_retry_status`.
- **The final attempt fails.** No retry is announced after it; the turn fails with the full message, status and request id included. Task 2 test `no_retry_is_announced_after_the_last_attempt`.
- **A session file written with the new fields, read by the current types with the fields removed.** Old readers ignore unknown fields: serialize with the fields, then deserialize into a mirror struct without them. Task 1 test `provider_error_json_only_grows`.

---

### Task 1: Classify by status; keep status and request id; plain refusal messages

**Files:**
- Modify: `crates/orca-core/src/provider_types.rs`. Add the two fields and a constructor that sets them.
- Modify: `crates/orca-provider/src/deepseek_http.rs` and `http_client.rs`. Capture the status and header where the non-2xx response is read; classify by status; build the messages.
- Test: the existing classification tests in `deepseek_http.rs`, plus its local fake HTTP server tests.

**Interfaces:**
- Produces: `ProviderError { kind, message, http_status: Option<u16>, request_id: Option<String> }` and `ProviderError::with_http(kind, message, http_status: Option<u16>, request_id: Option<String>) -> Self`.

- [ ] **Step 1: Write the failing tests.** Use the module's fake server helpers for HTTP replies.
  - `status_decides_the_kind`: 429 → `RateLimit`; 503 → `Server`; 400 with a context-length body → `ContextExceeded`; 401, 402, 403 and 404 → `Other`. The error carries `http_status`.
  - `a_client_error_mentioning_500_is_not_a_server_error`: 400 with body `max 5000 tokens per request` → `Other`, `is_retryable() == false`.
  - `request_id_prefers_x_request_id`: headers `x-request-id: a` and `request-id: b` give `Some("a")`. Only `x-deepseek-request-id: c` gives `Some("c")`.
  - `rejected_key_and_balance_messages`:
    - 401 → the message starts with `DeepSeek rejected the API key` and ends with `(HTTP 401, request id <id>)`;
    - 402 → it starts with `DeepSeek reports an insufficient account balance`;
    - a 400 body containing `Insufficient Balance` → the same balance wording.
  - `provider_error_json_only_grows`: `ProviderError::other("x")` serializes without the new keys, and one with both set deserializes into a struct that has only `kind` and `message`.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-core --lib provider_types && cargo test -p orca-provider --lib -- --test-threads=1 status_decides request_id rejected_key client_error_mentioning`
If the multi-filter form is refused, run each filter separately.
Expected: FAIL.

- [ ] **Step 3: Implement.** Text classification remains only for failures without a status, and it requires a number to be followed by a space, end or punctuation in a status-like position such as `HTTP 503` or `status 503`. The simplest correct rule is acceptable: drop the bare-number checks and rely on status.
- [ ] **Step 4: Run the tests to verify they pass.** Use the Step 2 commands plus `cargo test -p orca-provider --lib -- --test-threads=1`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/orca-core/src/provider_types.rs crates/orca-provider/src
git commit -m "fix(provider): errors are classified by HTTP status and carry the status and request id"
```

### Task 2: Retries are visible; the context meter shows one request

**Files:**
- Modify: `crates/orca-runtime/src/provider_turn.rs`. Report each scheduled retry with attempt, maximum, delay, kind and reason before `retry_budget.wait_blocking`. Use `attempt_usage.input_tokens` for `context_updated`.
- Modify: the provider-attempt path through `runtime_host.rs` and `runtime_actor/thread_actor_generation.rs` (`commit_surface_provider_attempt_failure`). Carry the retry information. Check whether the existing surface event for a failed attempt can carry it before adding a new one.
- Modify: the TUI status line, and the JSONL event writer for `orca exec`.
- Modify: `docs/harness-contract.md`, the JSONL docs under `site/src/docs/md/en` and `site/src/docs/md/zh` that list events, and the runtime-surface contract and its pins if a surface type changes.

**Interfaces:**
- Consumes: `ProviderError` from Task 1.
- Produces:
  - The retry report: `{ attempt: u32, max_attempts: u32, delay_ms: u64, kind: ProviderErrorKind, reason: String }`. `reason` is the error message cut to 200 chars.
  - It is shown in the TUI as `Retrying in <n>s (attempt <k> of <max>): <reason>`, and emitted in JSONL as an event, suggested name `provider.retry`.

- [ ] **Step 1: Write the failing tests:**
  - `a_retry_is_reported_before_the_backoff`: a fake server answers 503 once, then succeeds. The turn's event stream holds one retry report, `attempt` 1 and `max_attempts` 5, and the turn completes.
  - `no_retry_is_announced_after_the_last_attempt`: always 503, so exactly 4 retry reports, then a failed terminal whose message has `(HTTP 503`.
  - `a_cancel_during_backoff_clears_the_retry_status`: a TUI reducer test. After a retry report and then a cancel, the status line no longer shows `Retrying`.
  - `the_context_meter_counts_one_attempt`: the failed attempt reports usage with `input_tokens` 1000, and the successful one also with 1000. The `context_updated` value is 1000, while the usage totals count both.
  - `jsonl_exec_emits_provider_retry`: the exec JSONL contract test (follow the existing JSONL contract tests) sees the event with the five fields.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-runtime --lib retry -- --test-threads=1`, the TUI reducer test by name, and the JSONL contract test by name.
Expected: FAIL.

- [ ] **Step 3: Implement**, reusing the attempt-failure plumbing.
- [ ] **Step 4: Update the docs and contract pins.** Then run:

```bash
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS npm --prefix site ci
env -u NODE_OPTIONS npm --prefix site run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 4
env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1 retry context
```

Expected: every command exits 0, and clippy prints no warnings.

- [ ] **Step 5: Commit**

```bash
git add -A crates docs site/src
git commit -m "feat(runtime): retries are shown while Orca waits, and the context meter counts one request"
```
