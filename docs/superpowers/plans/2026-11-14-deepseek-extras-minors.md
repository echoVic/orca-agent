# DeepSeek Extras Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the minors the final review of the DeepSeek API extras batch deferred: OCR text cleanup, page numbers, error shapes, upload memory, repeated recognitions, the doctor's 403 wording, and two doc/comment gaps.

**Architecture:**
- OCR parsing and the multipart upload stay in `crates/orca-tools/src/deepseek_web.rs`.
- `read_document.rs` gains a per-file "one recognition at a time" gate on its in-memory cache.
- A new `ocr_disk_cache.rs` keeps recognized text under `ORCA_HOME/ocr_cache/`, keyed by the file's content, as `orca-provider/src/summary_cache.rs` does for summaries.
- The stream fix lives in `orca-provider/src/streaming.rs`, and the doctor wording in `orca-runtime/src/diagnostics.rs`.

**Tech Stack:** Rust (regex, serde, sha2, uuid, std `Condvar`), MDX docs.

**Spec:** None. On 2026-11-14 the user asked to fix the deferred minors ("继续处理那些小问题吧"). The list comes from the final review of branch feat/deepseek-api-extras, merged in local main 3a1d3362, and its items are restated under each task. Where the reviewer left the design open, the choice is Claude's and goes in the ledger.

## Global Constraints

- Every HTTP client is built with `orca_mcp::http::client_builder()` or `blocking_client_builder()`, which the http-client-boundary validator enforces.
- No crate may enter Cargo.lock. orca-tools may add `sha2 = { workspace = true }`, which the workspace already pins at 0.11.
- The DeepSeek key is never printed and never written into a test or command. Tests use `sk-test`.
- Tests make no live network calls. Fixtures bind `127.0.0.1:0` and call `set_nonblocking(false)` after accept; `crate::test_http` already does both.
- Windows compiles only in CI. Use std APIs that exist on every platform (`File::set_modified`, `fs::rename`), and check the `#[cfg]` attributes around any inserted item.
- Run cargo as `env -u NODE_OPTIONS cargo …` and put test filters after `--`. The orca-runtime lib has parallel-only flakes (`domain_policy`, `workspace_observation`), so verify those serially.
- Exact strings:
  - header with missing pages: `[Text recognized by DeepSeek OCR from {file name}: {n} of {page_count} page(s); DeepSeek returned no text for the rest]`
  - page marker: `--- Page {number} ---`, where number is DeepSeek's `index + 1`
  - a blank OCR body: `DeepSeek OCR closed the connection without a result`
  - doctor 403 detail: `{url} refused the request (HTTP 403): the account may lack access, or a firewall, region block or gateway refused it`
  - doctor 403 remedy: `check the key's account and any proxy or gateway in between, then rerun \`orca doctor --online\``
- Disk cache:
  - It lives in `ORCA_HOME/ocr_cache/<key>.json`, where key = lowercase hex SHA-256 of `orca-ocr-v1\npurpose:text_recognition\n` followed by the file bytes.
  - The total is capped at 64 MiB; the least recently used entries go first.
  - Only successful recognitions are stored.

## Review Focus

1. Two processes store the same cache entry at once: each writes to a temp file and renames it into place, so a reader sees a complete entry or none. Pinned by `concurrent_stores_of_one_entry_leave_it_whole` (Task 6).
2. The recognition others are waiting for fails or is cancelled: waiters wake, and one of them tries again; nobody hangs. Pinned by `a_failed_recognition_lets_the_next_read_try_again` and `a_waiting_read_can_be_cancelled` (Task 5).
3. `ORCA_HOME` is missing or can't be written: the disk cache is skipped silently and OCR still answers. Pinned by `the_disk_cache_is_skipped_when_it_cannot_be_written` (Task 6).
4. Page text with a literal `<` or `[` next to a box (code, math, tables): cleanup must not remove real text. Pinned by the extra cases in `ocr_text_drops_layout_tokens` (Task 1).
5. A file changes while it is read: the disk key is computed from the bytes actually uploaded, never from metadata. Pinned by `the_content_key_comes_from_the_uploaded_bytes` (Task 6).

---

### Task 1: Layout cleanup survives cut-short and unclosed boxes

**Files:** Modify `crates/orca-tools/src/deepseek_web.rs` (`LAYOUT_SPAN`, `REFERENCE_TAG`, `ocr_text`, and its test).

**Interfaces:** Produces the same `pub(crate) fn ocr_text(raw: &str) -> String`.

- [ ] **Step 1: Extend `ocr_text_drops_layout_tokens`.** Keep the three existing assertions and add these:
  - `"Outlook<｜box｜>[[52, 120, 9"` → `"Outlook"` (a page cut short inside the coordinates)
  - `"body <｜box｜>[[1, 2, 3, 4]] text <｜ref｜>X<｜/ref｜><｜box｜>[[5, 6, 7, 8]]<｜/box｜> tail"` → `"body  text X tail"` (an unclosed box keeps the text after it)
  - `"<｜ref｜>A<｜/ref｜><｜box｜>[[1, 2, 3, 4], [5, 6, 7, 8]]<｜/box｜>"` → `"A"` (several regions)
  - `"Outlook<｜bo"` → `"Outlook"`, and `"Outlook<｜/re"` → `"Outlook"` (cut inside a tag)
  - `"x < y and [1, 2] <｜ref｜>A<｜/ref｜><｜box｜>[[1, 2, 3, 4]]<｜/box｜> z"` → `"x < y and [1, 2] A z"` (Review Focus 4)
  - `"a <|b and c<|"` → unchanged (ASCII `<|` at the end can be code; only the fullwidth bar is trimmed)
- [ ] **Step 2: Run it.** `env -u NODE_OPTIONS cargo test -p orca-tools --lib -- ocr_text_drops_layout_tokens`. Expected: FAIL on the cut-short case.
- [ ] **Step 3: Implement.** Apply four patterns in order:
  1. Layout span: `<[|｜](?:box|det)[|｜]>[\d\s,.\[\]]*(?:<[|｜]/(?:box|det)[|｜]>)?`, the opening tag, the coordinate characters (possibly truncated) and an optional closing tag.
  2. Stray closing tag: `<[|｜]/(?:box|det)[|｜]>`.
  3. The existing reference tag.
  4. A trailing partial fullwidth token: `<｜/?[a-z]*｜?\z`.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit:** `fix(tools): keep text around cut-short and unclosed OCR boxes`

### Task 2: OCR keeps DeepSeek's page numbers and reports odd bodies

**Files:** Modify `crates/orca-tools/src/deepseek_web.rs` and `crates/orca-tools/src/read_document.rs`.

**Interfaces:** Produces:
- `pub(crate) struct OcrResult { pub(crate) pages: Vec<OcrPage>, pub(crate) page_count: Option<usize> }`
- `OcrPage { pub(crate) number: usize, pub(crate) text: String, pub(crate) complete: bool }`
- `ocr(…) -> Result<OcrResult, DeepSeekWebError>`
- the read_document cache holds `Arc<OcrResult>`

Step 1 tests:
- [ ] **Step 1a: Update `ocr_pages_keep_their_order_and_say_when_one_is_cut_short`.** It now expects `OcrResult { pages: [number 1 "first" complete, number 2 "second" incomplete], page_count: None }`.
- [ ] **Step 1b: `ocr_numbers_pages_by_their_index`.**
  - indices 0 and 2 with `"page_count": 3` → numbers 1 and 3, `page_count: Some(3)`
  - two pages with no `index` → numbers 1 and 2 (positions)
- [ ] **Step 1c: `ocr_reports_a_bare_string_error`.** The body `{"error":"OCR did not finish in time"}` → `Failed("OCR did not finish in time")`. In `deepseek_http_errors_map_to_readable_messages`, HTTP 500 with `{"error":"boom"}` → `"DeepSeek returned HTTP 500: boom"`.
- [ ] **Step 1d: `ocr_reports_a_result_that_never_came`.** The body `"\n\n\n"` → `Failed("DeepSeek OCR closed the connection without a result")`.
- [ ] **Step 1e: `ocr_ignores_an_empty_error_field`.** `"error"` set to `null`, `false`, `""` or `{}` next to `pages` → `Ok`.
- [ ] **Step 1f: `read_document_says_when_pages_are_missing`.** `page_count` 3 with pages at indices 0 and 2 → output `"[Text recognized by DeepSeek OCR from report.pdf: 2 of 3 page(s); DeepSeek returned no text for the rest]\n\n--- Page 1 ---\nA\n\n--- Page 3 ---\nC\n"`.
- [ ] **Step 2: Run** `env -u NODE_OPTIONS cargo test -p orca-tools --lib -- ocr read_document`. Expected: compile errors for `OcrResult`/`number`, then failures.
- [ ] **Step 3: Implement.**
  - Numbering: after sorting by `index`, a page's number is `index + 1` when the indices strictly increase, and `position + 1` otherwise.
  - Errors: `error` counts only when it is `true`, a non-blank string, or a non-empty object or array. `deepseek_message` takes a string `error` as the message itself, else `error.message`.
  - A body that is blank after trimming is the "closed the connection" failure.
  - `render_ocr` writes the missing-pages header when `page_count > pages.len()` and uses `page.number` in markers and in the cut-short note.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit:** `fix(tools): number OCR pages as DeepSeek does and report odd bodies`

### Task 3: A stream chunk with an empty error field is not an error

**Files:** Modify `crates/orca-provider/src/streaming.rs` (the `chunk.error` check in `push_line`, around line 302).

**Interfaces:** Produces private `fn reports_error(value: &serde_json::Value) -> bool`. It is true for `true`, a non-blank string, a non-empty object or array, and any number other than 0.

- [ ] **Step 1: Write `a_stream_whose_chunks_carry_an_empty_error_field_completes`.** Three content chunks carry `"error": false`, `"error": ""` and `"error": {}`, followed by `[DONE]`. The stream yields the joined content and no error. Keep `a_stream_error_chunk_reports_deepseeks_reason` unchanged.
- [ ] **Step 2: Run** `env -u NODE_OPTIONS cargo test -p orca-provider --lib -- stream`. Expected: FAIL. The new test errors with "the stream ended with an error".
- [ ] **Step 3: Implement.** Filter `chunk.error` through `reports_error` before returning `stream_error_message`.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit:** `fix(provider): ignore empty error fields in stream chunks`

### Task 4: An upload holds the file in memory once

**Files:** Modify `crates/orca-tools/src/deepseek_web.rs` (form building, `ocr`) and `crates/orca-tools/src/read_document.rs` (the read).

**Interfaces:**
- Produces `pub(crate) struct OcrUpload` (private fields: the form `Vec<u8>`, the boundary, and the file's byte range in the form).
- Produces `pub(crate) enum OcrUploadError { Io(std::io::Error), TooLarge(u64) }`.
- Produces `impl OcrUpload`:
  - `pub(crate) fn read(file_name: &str, reader: impl std::io::Read, expected_len: u64, max_len: u64) -> Result<Self, OcrUploadError>`
  - `fn read_with_boundaries(…, next_boundary: impl FnMut() -> String)`, private, for tests
  - `pub(crate) fn file_bytes(&self) -> &[u8]`
- `ocr` becomes `ocr(access: &DeepSeekWebAccess, upload: OcrUpload, should_cancel: &dyn Fn() -> bool) -> Result<OcrResult, DeepSeekWebError>`.

- [ ] **Step 1: Write the tests.**
  - Port `ocr_posts_multipart_file_and_purpose` and `ocr_file_names_cannot_break_out_of_their_header` to build their upload with `OcrUpload::read`. Same assertions.
  - Add `ocr_upload_picks_a_boundary_the_file_does_not_contain`: candidates `"orca-ocr-A"` then `"orca-ocr-B"`, with file bytes `b"x orca-ocr-A y"`. The form's parts parse with boundary `orca-ocr-B`, and the file part's content is `x orca-ocr-A y`.
  - Add `ocr_upload_refuses_a_file_past_the_limit`: an 11-byte reader with `max_len` 10 → `Err(TooLarge(11))`.
  - Add `ocr_upload_reads_into_one_buffer`: after `read` with an exact `expected_len`, `form.capacity() < form.len() + 64`.
- [ ] **Step 2: Run** `env -u NODE_OPTIONS cargo test -p orca-tools --lib -- ocr`. Expected: compile errors for `OcrUpload`.
- [ ] **Step 3: Implement.**
  - Boundaries are `orca-ocr-{uuid::Uuid::new_v4().simple()}`, a fixed width.
  - Write the part headers first, then `take(max_len + 1).read_to_end` the file straight after them, into capacity reserved for headers + `expected_len` + trailer + 32.
  - If the file slice contains the boundary (a `regex::bytes` literal search), rewrite the headers in place with the next candidate. The length doesn't change, because the width is fixed.
  - Append the closing delimiter.
  - `read_document` passes its open file and its metadata length.
- [ ] **Step 4: Run** `env -u NODE_OPTIONS cargo test -p orca-tools --lib -- ocr read_document`. Expected: PASS.
- [ ] **Step 5: Commit:** `perf(tools): read an OCR upload straight into its form`

### Task 5: One recognition per file at a time

**Files:**
- Modify `crates/orca-tools/src/read_document.rs`: the cache becomes `LazyLock<(Mutex<OcrCache>, Condvar)>`, `OcrCache` gains `pending: HashSet<OcrKey>`, and `OcrKey` derives `Hash`.
- Modify `crates/orca-tools/src/test_http.rs`: add `pub(crate) fn serve_slowly(delay: Duration, responses: Vec<(u16, String)>) -> Fixture`, which sleeps `delay` before each reply.

**Interfaces:**
- Consumes `OcrUpload` (Task 4) and `OcrResult` (Task 2).
- Produces a private `struct PendingRecognition` guard. On drop it removes its key from `pending` and calls `notify_all`.

- [ ] **Step 1: Write the tests.**
  - `parallel_reads_of_one_document_share_one_recognition`: `serve_slowly(600 ms, one reply)`. Thread A reads `deck.pdf`, and thread B reads the same file 100 ms later. Both complete with identical output, and `fixture.requests().len() == 1`.
  - `a_waiting_read_can_be_cancelled`: A is in flight against `serve_slowly(800 ms)`. B's `should_cancel` turns true 100 ms after B starts, so B returns `Cancelled` in under 600 ms, and A still completes.
  - `a_failed_recognition_lets_the_next_read_try_again`: the first read gets HTTP 500, and a second read of the same file against a new fixture completes.
- [ ] **Step 2: Run** `env -u NODE_OPTIONS cargo test -p orca-tools --lib -- read_document`. Expected: FAIL. In the parallel test, B's request finds the fixture gone.
- [ ] **Step 3: Implement.**
  - Under the lock, loop: a cache hit renders; a free key becomes this read's pending entry; otherwise `wait_timeout` 25 ms and check `should_cancel`.
  - Read, recognize and insert while holding the guard. The guard is dropped on every path: success, failure, cancel or panic.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit:** `fix(tools): share one OCR recognition between parallel reads of a file`

### Task 6: Recognized text is cached on disk across processes

**Files:**
- Create `crates/orca-tools/src/ocr_disk_cache.rs`, declared `mod ocr_disk_cache;` in `lib.rs`.
- Modify `crates/orca-tools/Cargo.toml` (`sha2`) and `crates/orca-tools/src/read_document.rs`.
- Modify `site/src/docs/md/{en,zh}/configuration.mdx`, the "Reading documents and images" bullets.

**Interfaces:**
- Consumes `OcrUpload::file_bytes` (Task 4). `OcrResult` and `OcrPage` derive `Serialize` and `Deserialize`.
- Produces:
  - `pub(crate) fn content_key(file_bytes: &[u8]) -> String`
  - `pub(crate) fn load(key: &str) -> Option<OcrResult>` and `pub(crate) fn store(key: &str, result: &OcrResult)`, which wrap the directory-taking versions below with `cache_dir()`
  - `fn load_in(dir: &Path, key: &str) -> Option<OcrResult>`, which also touches the entry's mtime
  - `fn store_in(dir: &Path, key: &str, result: &OcrResult, max_bytes: u64)`, which writes a temp file in `dir`, renames it into place, then prunes
  - `fn entry_path(dir: &Path, key: &str) -> PathBuf`, which is `dir/<key>.json`
  - `fn prune(dir: &Path, max_bytes: u64)`, which removes the oldest entries by mtime until the total is at most the cap
  - `const MAX_DISK_CACHE_BYTES: u64 = 64 * 1024 * 1024`
- `cache_dir()` is `orca_core::home::orca_home()/ocr_cache` in production. Under `#[cfg(test)]` it is the thread's override if one is set, else a thread-local `tempfile::TempDir`, as in `summary_cache.rs`. `#[cfg(test)] pub(crate) fn set_test_cache_root(root: Option<PathBuf>)` sets the override for the calling thread.
- Lookup order in `read_document`: memory cache, then the pending gate (Task 5), then the upload read, then the disk cache, then OCR. Store to disk and memory on success.

- [ ] **Step 1: Write the tests.**
  - `a_recognition_on_disk_serves_a_copy_of_the_file`: read `a.pdf` with a one-reply fixture, copy it to `b.pdf`, and read that. It completes with no second request, and the header names `b.pdf`.
  - `a_corrupt_disk_entry_is_recognized_again`: write `"not json"` at `entry_path(cache_dir, content_key(bytes))`. The read sends one request, and the entry is rewritten and parses.
  - `prune_keeps_the_newest_entries_under_the_cap`: four 10-byte entries with ascending mtimes in a temp dir and a cap of 25. The two newest remain.
  - `concurrent_stores_of_one_entry_leave_it_whole`: 8 threads `store_in` the same shared temp dir and key, then `load_in` returns the result, and no temp files remain (Review Focus 1).
  - `the_disk_cache_is_skipped_when_it_cannot_be_written`: `set_test_cache_root` to a regular file's path. `read_document` still completes from OCR (Review Focus 3).
  - `the_content_key_comes_from_the_uploaded_bytes`: `content_key(upload.file_bytes())` equals `content_key(original)` and differs for other bytes (Review Focus 5).
- [ ] **Step 2: Run** `env -u NODE_OPTIONS cargo test -p orca-tools --lib -- ocr_disk_cache read_document`. Expected: compile errors, then failures.
- [ ] **Step 3: Implement** to the interfaces above. Every disk error is ignored; the cache only ever saves a request.
- [ ] **Step 4: Update the docs.** In both languages, replace the "Orca keeps the text of the 8 documents…" bullet. The new text:
  - Recognized text is kept in memory (8 documents) and in `ORCA_HOME/ocr_cache` (up to 64 MiB, least recently used first), keyed by the file's content.
  - So a reread, a copy, a subagent or a later session doesn't pay again.
  - The same text is already in the session history.
  - Deleting the directory clears the cache.
- [ ] **Step 5: Run** `env -u NODE_OPTIONS cargo test -p orca-tools`. Expected: PASS.
- [ ] **Step 6: Commit:** `feat(tools): keep recognized document text on disk across processes`

### Task 7: The doctor's 403 wording, `--online` in the quickstart, and the child date comment

**Files:**
- Modify `crates/orca-runtime/src/diagnostics.rs` (`key_check_result` and its test).
- Modify `site/src/docs/md/{en,zh}/quickstart.mdx` (the "Check local readiness" section).
- Modify `site/src/docs/md/{en,zh}/cli-reference.mdx` (the `--online` paragraph).
- Modify `README.md` and `README.zh-CN.md` (the command list).
- Modify `crates/orca-runtime/src/child_agent_loop_setup.rs` (the `tell_child_today` doc comment).

**Interfaces:** None new.

- [ ] **Step 1: Extend `every_key_check_outcome_maps_to_a_credentials_online_check`.**
  - `Rejected(403)` → Fail, the detail is the exact 403 string, and the remedy does not contain "replace the key".
  - `Rejected(401)`'s remedy still contains "replace the key".
- [ ] **Step 2: Run** `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- diagnostics`. Expected: FAIL on the 403 case.
- [ ] **Step 3: Implement.** Branch `Rejected` on 401 vs 403, using the exact strings.
- [ ] **Step 4: Fix the docs and the comment.**
  - Quickstart, both languages: after the paragraph that says doctor does not call the provider, add that `orca doctor --online` also asks the provider whether it accepts the key, with one free `GET /models`.
  - cli-reference, both languages: a 403 fails as "refused", not as a rejected key.
  - READMEs: add an `orca doctor --online` line under `orca doctor`.
  - `tell_child_today`: children started through `execute_child_agent_loop` → `run_agent_loop` get the date again at every turn opening, which replaces this fragment. The helper matters for the `child_agent_loop_runner` path, which builds its own requests. A restored child gets the date after the restore, which requires an empty internal context.
- [ ] **Step 5: Run** the diagnostics tests again, plus `env -u NODE_OPTIONS cargo test -p orca-agent --test doctor_cli_contract` and the MDX compile of the four changed pages. Expected: PASS.
- [ ] **Step 6: Commit:** `fix(doctor): word a 403 as a refused request; document --online`
