# Session Newer-Version Health Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A session that a later Orca wrote is reported as `newer_version` with an "upgrade Orca" message instead of being quarantined. Records marked `"ignorable": true` are skipped, and kept byte for byte when the transcript is rewritten.

**Architecture:** The reader classifies a line that fails to parse before it gives up. An unknown type becomes `NewerVersion` or a skipped opaque record. A newer `schema_version` becomes `NewerVersion`. The rewrite path writes opaque records back verbatim.

**Tech Stack:** Rust, serde/serde_json, the JSONL thread store in `crates/orca-runtime/src/thread_store/`.

**Spec:** `docs/superpowers/specs/2026-11-27-session-newer-version-design.md`

## Global Constraints

- The healths that exist today keep their codes and meanings. A corrupt line still quarantines.
- The change to any JSON output that carries health is additive.
- Site docs change in both English and Chinese.
- Commit after each task; never push. No `TMPDIR`/`HOME` override; never touch `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.

## Review Focus

- **An ignorable line between two message records.** After a metadata change that rewrites the transcript, the file still has that line, unchanged, between the same two records. Task 2 test `a_rewrite_keeps_an_ignorable_line_in_place`.
- **An unknown type with `"ignorable": "yes"` or `1`.** Only the JSON boolean `true` counts; anything else is newer. Task 1 test `only_boolean_true_marks_a_record_ignorable`.
- **A newer session being listed, searched or exported.** Listing reports `newer_version`; search and export behave as they do for a quarantined session today, never as for a healthy one. Task 1 test `listing_reports_newer_version`.
- **A newer version line after a torn last line, or a torn line after a newer version line.** The first problem met wins, as with quarantine today. Task 1 test `the_first_problem_in_the_file_decides_its_health`.
- **`session.meta` with `schema_version` 1 plus an unknown field.** It reads as healthy. Task 1 test `session_meta_accepts_unknown_fields`.

---

### Task 1: The reader tells a newer session from a corrupt one

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/types.rs`. Add the health variant and `as_str`, and remove `deny_unknown_fields` from `SessionMeta`. Add a doc comment on `SessionRecord` with the rule for future types.
- Modify: `crates/orca-runtime/src/thread_store/reader.rs` and `writer.rs` (`parse_session_record`, `session_health_error` Display).
- Modify: every `match` on `StoredSessionHealth` (the compiler will list them) and the places that show health to users.
- Test: `crates/orca-runtime/src/thread_store/storage_tests.rs`, or the tests module that holds today's quarantine tests.

**Interfaces:**
- Produces:
  - `StoredSessionHealth::NewerVersion`, with `as_str() == "newer_version"`.
  - An opaque record for skipped lines. Its exact form is the builder's choice, but Task 2 consumes it: the rewrite path must receive the line's original text.

- [ ] **Step 1: Write the failing tests.** Each writes a JSONL transcript by hand: a valid `session.meta`, one `conversation.message`, then the line under test, then another `conversation.message`. Follow the existing quarantine tests for how they build and open such files.
  - `an_unknown_record_type_reports_newer_version`: line `{"type":"future.thing","x":1}`. Health is `NewerVersion`, the code is `unknown_record_type`, and the error text contains `newer version of Orca`, `future.thing` and `upgrade`.
  - `an_ignorable_unknown_record_is_skipped`: the same with `"ignorable":true`. Health is `Healthy`, and both messages load.
  - `only_boolean_true_marks_a_record_ignorable`: `"ignorable":"yes"` and `"ignorable":1` both give `NewerVersion`.
  - `a_newer_schema_version_reports_newer_version`: `session.meta` with `schema_version` 2. Health is `NewerVersion` and the code is `newer_schema_version`.
  - `session_meta_accepts_unknown_fields`: `{"type":"session.meta","schema_version":1,"added_later":true}`. Health is `Healthy`.
  - `a_known_type_with_bad_fields_still_quarantines`: `{"type":"conversation.message","message":42}`. Health is `Quarantined`, with today's code.
  - `the_first_problem_in_the_file_decides_its_health`: an unknown non-ignorable line followed by garbage gives `NewerVersion`; garbage followed by an unknown line gives `Quarantined`.
  - `listing_reports_newer_version`: the session listing used by `orca sessions` or the TUI picker shows health `newer_version` for such a session.
  - `resume_of_a_newer_session_is_refused_with_the_upgrade_message`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-runtime --lib thread_store -- --test-threads=1`
Expected: the new tests FAIL. `NewerVersion` does not exist, and unknown types quarantine.

- [ ] **Step 3: Implement the classification.** Classify only after the existing parse fails, so known records take today's path. The opaque record is ignored by every fold and projection.

- [ ] **Step 4: Run the tests to verify they pass.** Use the Step 2 command. Expected: PASS, the existing thread_store tests included.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src
git commit -m "fix(session): a session written by a newer Orca is reported as newer, not corrupt"
```

### Task 2: Rewrites keep the lines they skipped; docs

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/writer.rs` (`rewrite_records_unlocked`), plus any other path that writes a transcript back from records it read. Search for callers of the record iterator that then write.
- Modify: `site/src/docs/md/en/session-commands.mdx`, `site/src/docs/md/zh/session-commands.mdx`, and `docs/harness-contract.md` if it lists session healths.

**Interfaces:**
- Consumes: the opaque record from Task 1.

- [ ] **Step 1: Write the failing test** `a_rewrite_keeps_an_ignorable_line_in_place`.
  - Set up a transcript with an ignorable unknown line between two messages.
  - Perform an operation that rewrites the transcript, such as a rename or another metadata update that reaches `rewrite_records_unlocked`.
  - Then read the file. The ignorable line is present byte for byte, between the same two messages.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p orca-runtime --lib a_rewrite_keeps_an_ignorable_line_in_place -- --test-threads=1`
Expected: FAIL. The line is dropped.

- [ ] **Step 3: Implement.** The rewrite writes an opaque record's original text unchanged.

- [ ] **Step 4: Update the docs.**
  - Session-commands, in English and Chinese:
    - a session written by a newer Orca is shown as newer and is not corrupt;
    - upgrading opens it;
    - records a newer version marks as skippable are read past.
  - The harness contract, if it lists healths: add `newer_version`.

- [ ] **Step 5: Verify**

Run:

```bash
cargo test -p orca-runtime --lib thread_store -- --test-threads=1
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS npm --prefix site ci
env -u NODE_OPTIONS npm --prefix site run build
cargo fmt --all -- --check
cargo clippy -p orca-runtime --all-targets --locked
```

Expected: every command exits 0, and clippy prints no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src site/src/docs docs/harness-contract.md
git commit -m "fix(session): rewriting a transcript keeps the records it skipped"
```
