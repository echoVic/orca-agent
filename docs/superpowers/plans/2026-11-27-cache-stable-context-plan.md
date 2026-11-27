# Cache-Stable Context Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two changes that keep context in DeepSeek's prompt cache.
- **Overlay.** Internal-context fragments other than `plan` are appended to history only when they change, instead of being re-sent uncached at the tail of every request.
- **Resume.** A resumed or forked session reuses its recorded system prompt and appends only the sections that changed.

**Architecture:**
- **Fragments.** `render_internal_context` (`crates/orca-provider/src/deepseek_http.rs`) keeps rendering only `plan` at the tail. The runtime compares the other fragments each step with the last versions appended in this thread. Changed or removed fragments go into one system message, recorded with the existing system-message record and placed before the request.
- **System prompt.** It is built from named sections (`system_prompt.rs`, `agent_common.rs`, `instructions.rs`), and the section texts are recorded when the session starts. Resume and fork compare the rebuilt sections with the recorded ones and append a change notice per changed section.

**Tech Stack:** Rust: orca-core `conversation.rs` (fragments), orca-provider `deepseek_http.rs`, orca-runtime (`session.rs`, `history.rs`, `agent_common.rs`, `system_prompt.rs`, `compaction.rs`).

**Spec:** `docs/superpowers/specs/2026-11-27-cache-stable-context-design.md`

## Global Constraints

- **Fragment messages.**
  - Header text, exactly: `Current {kind} context (this replaces any earlier version):`
  - Removal text: `The {kind} context no longer applies.`
  - Section-change text: `The "{section}" instructions changed; they now read:` followed by the new text.
  - Section-removal text: `The "{section}" instructions no longer apply.`
- **What changes and what stays.**
  - `plan` stays at the tail.
  - After compaction, the current versions of all appended fragments are appended again after the summary.
  - Sessions recorded before this change (no recorded sections) resume exactly as today: the prompt is rebuilt.
- **Compatibility.**
  - Every new record must be readable by Orca 0.5.25: existing system-message records, or optional fields on records that do not deny unknown fields.
  - Never write new `session.meta` fields.
  - Never add closed-enum variants.
- **Tests and measurement.**
  - Measure uncached prompt tokens per request before and after on a scripted multi-step session, and report them.
  - Prompt size tests must pass.
- **Process.**
  - Commit after each task and never push.
  - No `TMPDIR` or `HOME` override, and never touch `~/.orca`.
  - Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo tests.

## Review Focus

- **A fragment that changes every step** (for example memory refreshed each turn with new content). Each change appends once. There is no growth when the content is unchanged. Test `an_unchanged_fragment_appends_nothing`.
- **Thinking mode.** A fragment message mid-history keeps the request valid. Assistant messages after the last user message keep `reasoning_content`. Test `fragment_messages_keep_thinking_mode_requests_valid`.
- **Compaction removing the last appended version.** The current version is re-appended after the summary, exactly once. Test `compaction_reappends_current_fragments_once`.
- **Resume after an Orca upgrade that changed one prompt section.** The recorded prompt is sent, plus one change notice. The cached prefix up to the history is byte-identical to the recorded request. Test `resume_after_a_prompt_change_appends_one_notice`.
- **Fork into a worktree with a different cwd.** The cwd section is reported as changed, and nothing else changes. Test `fork_reports_only_the_changed_cwd_section`.

---

### Task 1: Fragments other than plan are appended when they change

- [ ] **Step 1: Write the failing tests:**
  - `an_unchanged_fragment_appends_nothing`
  - `a_changed_fragment_appends_one_replacement`
  - `a_removed_fragment_appends_the_no_longer_applies_note`
  - `plan_stays_at_the_tail`
  - `compaction_reappends_current_fragments_once`
  - `fragment_messages_keep_thinking_mode_requests_valid`
  - `resume_rebuilds_the_last_appended_state_without_new_messages`
  - `uncached_tail_tokens_drop_on_a_scripted_session`: count the tail bytes per request before and after.
- [ ] **Step 2:** Run each test and confirm it fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p orca-provider --lib -- --test-threads=1` and `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1`, and confirm both pass.
- [ ] **Step 5:** Commit `feat(context): context that changes rarely is appended once instead of re-sent every request`.

### Task 2: A resumed session keeps its system prompt

- [ ] **Step 1: Write the failing tests:**
  - `the_system_prompt_sections_are_recorded_at_session_start`
  - `resume_with_an_unchanged_prompt_sends_the_recorded_prompt`
  - `resume_after_a_prompt_change_appends_one_notice`
  - `a_removed_section_appends_the_no_longer_applies_note`
  - `fork_reports_only_the_changed_cwd_section`
  - `a_session_without_recorded_sections_resumes_as_before`
  - `the_recorded_sections_are_readable_by_the_previous_release`: open with the v0.5.25 types.
- [ ] **Step 2:** Run each test and confirm it fails.
- [ ] **Step 3:** Implement.
  - Record the sections with an optional field on a record that 0.5.25 reads with unknown fields allowed, or with a system-message record. Do not use `session.meta`.
- [ ] **Step 4:** Run the crate tests and confirm they pass.
- [ ] **Step 5:** Commit `feat(session): a resumed session keeps its system prompt and appends only what changed`.

### Task 3: Docs and checks

- [ ] Update the prefix-cache and context docs, English and Chinese.
- [ ] Run the workspace checks:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -j 4`
  - the runtime-surface validator
  - the Windows platform-boundary validator
  - the site build
- [ ] Commit `docs: context sent once and resumed prompts`.
