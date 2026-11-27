# Spill Large Results and Nested Instructions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement parts 1 and 4 of the spec:
- a tool result larger than 12,500 tokens is saved whole to a file in the session's directory, and the model gets the head and tail plus a pointer that `read_file` and `grep` can follow;
- `AGENTS.md` files in subdirectories are found when the model touches files below them;
- instruction chains get byte budgets.

Parts 2 (skill catalog) and 3 (on-demand MCP) are separate work. Do not implement them here.

**Architecture:**
- **Spill.** A spill step runs where a completed tool result is turned into the model-visible result for MCP, external, `web_fetch` and `web_search` results. It writes `<session dir>/spill/<tool-call-id>.txt` and replaces the result text. `read_file` and `grep` accept that directory as a read-only exception to the workspace rule.
- **Nested instructions.** These are tracked per thread. After a successful `read_file`, `write_file`, `edit` (or `apply_patch`, if present) on a path, the runtime checks the directories between the workspace root and that path for `AGENTS.md`, and appends any it has not appended before.

**Tech Stack:** Rust: orca-tools (`registry.rs`, `read_file.rs`, `grep.rs`, `lib.rs` `resolve_workspace_path`), orca-runtime (`instructions.rs`, the tool-result path, the session directory under `ORCA_HOME`).

**Spec:** `docs/superpowers/specs/2026-11-27-discoverability-and-large-output-design.md` (parts 1 and 4)

## Global Constraints

- **Spill.**
  - Threshold: 12,500 tokens.
  - Notice text, exactly: `[{omitted} bytes omitted. The full result is saved at {path}; read it with read_file using offset and limit, or search it with grep.]`
  - Files are user-only (0600 on unix), under the session's own directory, and deleted with the session.
  - A storage failure keeps today's behaviour.
- **Nested instructions.**
  - Header, exactly: `Additional instructions for {dir}/ from {dir}/AGENTS.md:`.
  - A changed file is appended again as `Updated instructions for {dir}/ from {dir}/AGENTS.md:`.
  - The message is appended once before the next request, as a system message recorded in the transcript.
- **Budgets.** Each chain is limited to 65,536 bytes (user-global, project root, nested). The broadest files are dropped first and the most specific is cut last, and a note names what was left out.
- **Never spilled.** `read_file`, `grep`, `glob`, `task_read_output` and shell tasks.
- **Process.** Commit after each task and never push. Don't override `TMPDIR` or `HOME`, and never touch `~/.orca`; tests use a temp `ORCA_HOME`. Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo tests.

## Review Focus

- **The spill path exception.** It allows only the session's own spill directory, through `resolve_workspace_path`'s canonical check. A symlink inside it that points elsewhere is refused. Test `a_symlink_in_the_spill_dir_is_refused`.
- **A result exactly at the threshold.** Not spilled. One token over is spilled. Test `the_threshold_is_exclusive`.
- **The micro-compaction marker of a spilled result.** It keeps the path. Test `micro_compaction_keeps_the_spill_path`.
- **An `AGENTS.md` that the model itself writes during the session.** It is appended as updated instructions on its next touch, never in a loop. Test `an_agents_md_the_model_wrote_is_appended_once`.
- **Two touched paths under the same directory.** Its `AGENTS.md` is appended once. Test `nested_instructions_are_appended_once_per_file_version`.

---

### Task 1: Spill large tool results

- [ ] **Step 1: Write the failing tests:**
  - `a_large_mcp_result_is_saved_and_pointed_to`: a 200 KB MCP text result. The model-visible text holds the head, the tail and the exact notice, and the file holds the full text.
  - `a_large_web_fetch_result_is_saved`.
  - `the_threshold_is_exclusive`.
  - `read_file_reads_a_spill_file_with_offset_and_limit`.
  - `grep_searches_a_spill_file`.
  - `a_symlink_in_the_spill_dir_is_refused`.
  - `a_storage_failure_keeps_the_original_result`.
  - `micro_compaction_keeps_the_spill_path`.
  - `spill_files_are_removed_with_their_session`.
- [ ] **Step 2:** Run each by name → FAIL.
- [ ] **Step 3:** Implement.
  - Find where completed MCP, external and web results become model-visible.
  - Add the spill directory exception in `resolve_workspace_path`'s callers for `read_file` and `grep`, not in `resolve_workspace_path` itself. The other tools keep the strict rule.
- [ ] **Step 4:** Run `cargo test -p orca-tools --lib -- --test-threads=1` and the named runtime tests → PASS.
- [ ] **Step 5:** Commit `feat(tools): a tool result too large for the context is saved to a file the model can read`.

### Task 2: Nested AGENTS.md and instruction budgets

- [ ] **Step 1: Write the failing tests:**
  - `touching_a_file_below_a_nested_agents_md_appends_it_once`.
  - `nested_instructions_are_appended_once_per_file_version`.
  - `an_updated_nested_agents_md_is_appended_as_updated`.
  - `an_agents_md_the_model_wrote_is_appended_once`.
  - `each_instruction_chain_has_its_own_budget`: a 100 KB project `AGENTS.md` does not evict `~/.orca/AGENTS.md`, and the note names what was cut.
  - `the_root_chain_is_unchanged_when_it_fits`, so the system prompt bytes are unchanged for ordinary projects.
  - `nested_instructions_survive_resume_without_repeating`.
- [ ] **Step 2:** Run each by name → FAIL.
- [ ] **Step 3:** Implement.
  - Nested files are found only below the trusted workspace root, never above it, and the root's own `AGENTS.md` keeps its current place in the system prompt.
  - The appended message reuses the existing system-message transcript record, so older Orca versions read it.
- [ ] **Step 4:** Run `cargo test -p orca-runtime --lib instructions -- --test-threads=1` and the named tests → PASS.
- [ ] **Step 5:** Commit `feat(runtime): AGENTS.md files in subdirectories are found, and instructions are bounded`.

### Task 3: Docs and checks

- [ ] Document both in the site docs (`site/src/docs/md/en` and `zh`): the context and instructions pages.
- [ ] Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 4
env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS npm --prefix site ci && env -u NODE_OPTIONS npm --prefix site run build
```

- [ ] Commit `docs: large tool results and nested instructions`.
