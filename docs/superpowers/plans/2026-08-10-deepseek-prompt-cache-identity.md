# DeepSeek Prompt Cache Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DeepSeek prompt-cache compatibility deterministic, observable in durable session history, and isolated across forks.

**Architecture:** `orca-provider` computes a content-free checkpoint from its actual DeepSeek wire lowering and canonical tool payload. `orca-runtime` appends that checkpoint as audit-only JSONL immediately before provider dispatch; transcript reconstruction and fork construction never consume or copy it.

**Tech Stack:** Rust 2024, serde/serde_json, SHA-256, JSONL `SessionWriter`, focused Cargo tests, credential-gated DeepSeek API example.

---

### Task 1: Canonical DeepSeek Tool Payloads

**Files:**
- Modify: `crates/orca-provider/src/tool_schema.rs`

- [ ] **Step 1: Write the failing permutation tests**

Add tests that construct `alpha` and `zeta` definitions in opposite orders and
assert identical output from both `deepseek_tools_schema` and
`deepseek_strict_tools_schema_for_endpoint`.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p orca-provider tool_schema::tests::tool_definition_order_does_not_change --locked
```

Expected: FAIL because the returned vectors preserve insertion order.

- [ ] **Step 3: Implement canonical lowering**

Lower each definition, then sort the resulting tools by
`tool["function"]["name"]`. Apply strict normalization before the same sort so
strict and plain request payloads share the ordering contract.

- [ ] **Step 4: Verify GREEN**

Run the focused tool-schema tests and expect all to pass:

```bash
cargo test -p orca-provider tool_schema::tests --locked
```

### Task 2: Versioned Wire Checkpoint

**Files:**
- Create: `crates/orca-provider/src/prompt_cache.rs`
- Modify: `crates/orca-provider/src/lib.rs`
- Modify: `crates/orca-provider/src/deepseek_http.rs`

- [ ] **Step 1: Write failing checkpoint tests**

Define the desired public API in tests:

```rust
let checkpoint = checkpoint_for_deepseek_request(&base, &config).unwrap();
assert!(checkpoint.matches_deepseek_prefix(&extended, &config).unwrap());
assert!(!checkpoint.matches_deepseek_prefix(&changed_system, &config).unwrap());
assert!(!checkpoint.matches_deepseek_prefix(&extended, &changed_tools).unwrap());
```

Also assert serialized checkpoints contain hashes/counts but none of the known
prompt text.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p orca-provider prompt_cache::tests --locked
```

Expected: compilation fails because the module and checkpoint API do not exist.

- [ ] **Step 3: Implement the checkpoint**

Add a serde-compatible `PromptCacheCheckpoint` with `version`,
`scope_sha256`, `message_prefix_sha256`, `message_count`,
`tool_schema_sha256`, and `tool_count`. Hash domain-separated serialized bytes
from `conversation_to_api_messages` and the same sorted/capped strict-or-plain
tool payload used by the primary request. Add prefix verification by hashing the
first recorded number of lowered messages from the candidate request.

- [ ] **Step 4: Share request tool selection**

Extract one crate-private DeepSeek helper that selects and caps the primary
tool payload. Use it from both HTTP request construction and checkpoint
calculation so the identity cannot drift from the request body.

- [ ] **Step 5: Verify GREEN**

Run:

```bash
cargo test -p orca-provider prompt_cache::tests --locked
cargo test -p orca-provider tool_schema::tests --locked
```

Expected: all focused tests pass.

### Task 3: Durable Audit Record And Fork Isolation

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/types.rs`
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`
- Modify: `crates/orca-runtime/src/provider_turn.rs`
- Modify: `crates/orca-runtime/src/session.rs`

- [ ] **Step 1: Write failing persistence tests**

Add a writer test that appends a `provider.prompt_cache_checkpoint` record and
asserts transcript reconstruction leaves messages, usage, and completion state
unchanged. Add a session fork test that seeds a parent history with the record,
constructs the child using `start_writer_with_messages`, and asserts the child
JSONL has no `provider.prompt_cache_checkpoint` line.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p orca-runtime prompt_cache_checkpoint --locked
```

Expected: compilation fails because the session record and writer method do not
exist.

- [ ] **Step 3: Add the audit-only history record**

Add `SessionPromptCacheCheckpointRecord { turn_id, checkpoint, recorded_at }`,
the `SessionRecord::PromptCacheCheckpoint` serde tag, and
`SessionWriter::append_prompt_cache_checkpoint`. Explicitly ignore the variant
in transcript reconstruction and treat its hash/count fields as content-free in
redaction.

- [ ] **Step 4: Append at the provider ownership boundary**

In `RuntimeProviderTurnStep::run`, after final model-conversation construction
and before `orca_provider::start_streaming`, compute and append a checkpoint only
for `ProviderKind::DeepSeek` when a history writer exists. Map serialization
failure into `io::ErrorKind::InvalidData`; a failed append prevents dispatch so
the durable audit boundary cannot lag the request.

- [ ] **Step 5: Verify GREEN**

Run:

```bash
cargo test -p orca-runtime prompt_cache_checkpoint --locked
cargo test -p orca-runtime provider_turn --locked
```

Expected: all focused tests pass.

### Task 4: Real DeepSeek Cache-Hit Verifier And Gates

**Files:**
- Create: `crates/orca-provider/examples/prompt_cache_identity_realapi.rs`
- Modify: `docs/production-roadmap.md`

- [ ] **Step 1: Add the verifier**

Build a long deterministic system prefix, send one DeepSeek request, append its
assistant response and a second user message, then send the extension. Exit with
an error unless the second response has `usage.cache_tokens > 0`. Load the API
key using the existing real-API example convention; when no credential is
available, print an explicit skip and return success without exposing secrets.

- [ ] **Step 2: Run focused and all-target gates**

```bash
cargo test -p orca-provider --locked
cargo test -p orca-runtime prompt_cache_checkpoint --locked
cargo check -p orca-provider --all-targets --locked
cargo check -p orca-runtime --all-targets --locked
cargo run -p orca-provider --example prompt_cache_identity_realapi --locked
cargo fmt --all -- --check
git diff --check
```

Expected: local gates pass; the real verifier either observes a non-zero second
request cache hit or reports only the documented missing-credential skip.

- [ ] **Step 3: Update roadmap evidence and commit**

Record the completed deterministic identity, fork isolation, checkpoint tests,
and real-API result in `docs/production-roadmap.md`, then create one semantic
commit:

```bash
git add crates/orca-provider crates/orca-runtime docs
git commit -m "feat(provider): make DeepSeek cache identity explicit"
```
