# DeepSeek API Extras Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Three takeaways from DeepSeek's official chat demo, adapted to Orca:
- today's date moves out of the system prompt into a reminder that's added only when the date changes;
- `orca doctor --online` verifies the API key with the free `GET /models`;
- `read_file` reads PDF, Word, PowerPoint and image files through DeepSeek's OCR API.

**Architecture:**
- The date becomes a `[Date]` system message. Main-session turn openings add it when it's missing or stale, and child agents get it when their conversation is built.
- The key check is a blocking `GET {base}/models` helper in orca-provider. The doctor calls it only under `--online`.
- OCR is a `deepseek_web::ocr` call that sends a hand-built multipart body to `POST {origin}/v1/tools/ocr`. `read_file` routes document and image extensions to it when DeepSeek access exists, and caches results per file in the process.

**Tech Stack:** Rust (reqwest via `orca_mcp::http`, tokio, serde, chrono, regex), MDX docs.

**Spec:** None. On 2026-11-14 the user picked items 1–5 from Claude's review of `deepseek-ai/chat-demo` and said "你自己定" to the design questions. The decisions below are Claude's calls. Items 1 (stream error chunks) and 4 (local date for `fresh_days`) already landed on this branch (34d45e77, d37c83d1).

## Decisions

- **Date**:
  - The system prompt drops `- Today's date: …`, so it stays the same across days and a session resumed on a later day keeps its cached prefix.
  - The `[Date] Today's date is YYYY-MM-DD.` system message is added only when the newest one in the conversation is missing or names another day, so a session that crosses midnight learns the new date.
  - It is not pinned. If compaction drops it, it is added again.
- **Key check**:
  - `orca doctor` stays local by default.
  - `--online` adds a `credentials-online` check that calls `GET /models`. For the official host it calls the origin, otherwise `base_url + /models`, with the configured key and a 10 s timeout.
  - 200 passes, 401 and 403 fail, and anything else warns (unreachable, timeout, other status).
  - The TUI setup screen is left alone: it saves the key synchronously, so a check there needs a new async state. That is out of scope.
- **OCR**:
  - It lives inside `read_file`; there is no new tool.
  - It covers the extensions `pdf doc docx ppt pptx png jpg jpeg webp gif bmp tif tiff`, with `purpose=text_recognition`.
  - It requires the same `DeepSeekWebAccess` as the web tools (official endpoint + key + `[tools] deepseek_web`). The switch's docs now say it covers OCR too.
  - Without access, these files fail with a hint instead of the UTF-8 error.
  - Results are cached in the process per (canonical path, size, mtime), up to 8 documents, so offset/limit reads don't bill again.
  - The rendered text has a header line and `--- Page N ---` markers. The `<｜box｜>…<｜/box｜>` spans are removed and the `<｜ref｜>` tags are unwrapped. `offset`/`limit` apply to its lines.
  - Files over 100 MiB are refused before upload.

## Global Constraints

- Build every HTTP client with `orca_mcp::http::client_builder()` or `blocking_client_builder()`; this is the http-client-boundary validator.
- The DeepSeek key goes only to `https://api.deepseek.com` for OCR. For the key check it goes to the configured provider base, which is where it already goes.
- Never print the key.
- No new crate dependencies and no new reqwest features. The multipart body is built by hand.
- Tests make no live network calls. Fixtures bind `127.0.0.1:0` and call `set_nonblocking(false)` after accept.
- New public CLI options update `docs/public-cli-manifest.json`, and the en/zh `cli-reference.mdx`.
- Exact strings:
  - `[Date] Today's date is {YYYY-MM-DD}.`
  - OCR header: `[Text recognized by DeepSeek OCR from {file name}: {n} page(s)]`
  - page marker: `--- Page {n} ---`
  - no-access hint: `{path} is a {kind} file; Orca reads these through DeepSeek's OCR API, which needs the official DeepSeek endpoint (https://api.deepseek.com) with an API key and [tools] deepseek_web on.`

## Review Focus

1. A long session that crosses midnight: the next turn must add exactly one new `[Date]` message, and the history before it must not change.
2. A resumed session whose transcript has an old `[Date]` must get the current one at its first turn, and only once.
3. `orca doctor` without `--online` must make no network call.
4. A 300-page PDF read in several `offset`/`limit` chunks must hit OCR once.
5. OCR page text with `<｜box｜>[[x1, y1, x2, y2]]<｜/box｜>` coordinates must not reach the model.

---

### Task 1: Date reminder instead of the date in the system prompt

**Files:** `crates/orca-runtime/src/system_prompt.rs`, `crates/orca-core/src/conversation.rs` (helper + tests), `crates/orca-runtime/src/runtime_turn_opening.rs`, the child-agent conversation setup (`crates/orca-runtime/src/child_agent_loop_setup.rs` or `session.rs::bootstrap_agent_conversation_for_loop`)

**Interfaces:**
- Produces `Conversation::ensure_date_reminder(&mut self, today: chrono::NaiveDate) -> bool`, which returns true when it added a message.
- Produces `pub const DATE_REMINDER_PREFIX: &str = "[Date] Today's date is ";`.

- [ ] **Step 1: Failing tests**
  - `ensure_date_reminder_adds_once_per_day`: an empty conversation adds one message; the same day adds nothing; the next day adds one more. The newest reminder wins.
  - `system_prompt_has_no_date`: `build_system_prompt` doesn't contain `Today's date`.
  - `turn_opening_tells_the_model_today` (runtime): opening a turn adds the reminder; opening another turn the same day doesn't.
- [ ] **Step 2: Watch them fail.**
- [ ] **Step 3: Implement.** Scan the messages from the newest back for the first system message that starts with the prefix. Use `chrono::Local::now().date_naive()` at the call sites.
- [ ] **Step 4: Run** `cargo test -p orca-core` and `cargo test -p orca-runtime --lib system_prompt`, plus the new runtime test and the turn-opening tests.
- [ ] **Step 5: Commit** — `feat(runtime): tell the model today's date in a reminder, not the system prompt`

### Task 2: `orca doctor --online` key check

**Files:** `crates/orca-provider/src/` (new `key_check.rs` or inside `deepseek_http.rs`), `crates/orca-runtime/src/diagnostics.rs`, `src/cli.rs`, `docs/public-cli-manifest.json`, `site/src/docs/md/{en,zh}/cli-reference.mdx`

**Interfaces:**
- Produces `pub enum KeyCheck { Valid, Rejected(u16), Unexpected(u16), Unreachable(String) }`.
- Produces `pub fn check_api_key(base_url: Option<&str>, api_key: &str, timeout: Duration) -> KeyCheck`.
- Produces `DoctorOptions.online: bool` and `DoctorRequest.online: bool`.

- [ ] **Step 1: Failing tests**
  - provider: a fixture records `GET /models` with `authorization: Bearer sk-test`. 200 → `Valid`, 401 → `Rejected(401)`, 500 → `Unexpected(500)`, port 9 → `Unreachable`. The official-host URL drops any path.
  - diagnostics: every `KeyCheck` maps to a pass, fail or warn check with id `credentials-online`. `collect_doctor` without `online` has no `credentials-online` check.
  - cli: `orca doctor --online` parses, and the manifest test passes with `--online` listed.
- [ ] **Step 2: Watch them fail.**
- [ ] **Step 3: Implement.** No key means the check is skipped with a warning. Load the base URL the same way the provider does.
- [ ] **Step 4: Run** `cargo test -p orca-provider`, `cargo test -p orca-runtime --lib diagnostics` and `cargo test -p orca-agent cli`.
- [ ] **Step 5: Commit** — `feat(doctor): check the API key against /models with --online`

### Task 3: `read_file` reads documents and images through DeepSeek OCR

**Files:** `crates/orca-tools/src/deepseek_web.rs` (OCR call), `crates/orca-tools/src/read_file.rs`, `crates/orca-tools/src/registry.rs` (pass access to read_file), `crates/orca-core/src/config/mod.rs` (switch doc comment), `site/src/docs/md/{en,zh}/configuration.mdx`

**Interfaces:**
- Produces `pub(crate) struct OcrPage { text: String, complete: bool }`.
- Produces `pub(crate) async fn ocr(access: &DeepSeekWebAccess, file_name: &str, bytes: Vec<u8>, should_cancel: &dyn Fn() -> bool) -> Result<Vec<OcrPage>, DeepSeekWebError>`.
- `read_file::execute_or_cancel` gains `deepseek: Option<&DeepSeekWebAccess>` before `should_cancel`.

- [ ] **Step 1: Failing tests**
  - `ocr_posts_multipart_file_and_purpose`: the fixture records `POST /v1/tools/ocr`, the bearer key, `multipart/form-data`, a `purpose` part `text_recognition`, and a `file` part with the file name and bytes. Leading keep-alive newlines before the JSON still parse.
  - `ocr_text_drops_layout_tokens`: `<｜ref｜>Title<｜/ref｜><｜box｜>[[52, 40, 948, 96]]<｜/box｜>` → `Title`.
  - `read_file_reads_a_pdf_through_ocr`: the output starts with the header, has `--- Page 1 ---`, and a page cut short says so.
  - `read_file_ocr_is_cached`: two reads with offset/limit send one request.
  - `read_file_without_access_explains_documents`: a .pdf with no access fails with the hint.
  - `read_file_refuses_documents_over_100_mib`: checked from metadata, so no 100 MiB fixture is needed (use a sparse file or a size check helper).
- [ ] **Step 2: Watch them fail.**
- [ ] **Step 3: Implement.** Use a 180 s timeout and the same error mapping and masking as search and fetch.
- [ ] **Step 4: Run** `cargo test -p orca-tools`, the validators, clippy, fmt, and check that the docs compile.
- [ ] **Step 5: Commit** — `feat(tools): read PDF, Office and image files through DeepSeek OCR`
