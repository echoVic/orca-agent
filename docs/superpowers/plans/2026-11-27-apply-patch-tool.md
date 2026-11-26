# apply_patch Tool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new `apply_patch` tool. It takes `{"input": string}` in the `*** Begin Patch` … `*** End Patch` envelope, checks the whole patch before writing anything, then applies it in order.

**Architecture:** A parser and applier live in a new `crates/orca-tools/src/apply_patch.rs`. The tool is registered beside `edit` with the same write capability, approval action, sandbox profile, rewind pre-image capture and workspace rule. Its result has the shape `edit` uses for a change, one entry per file.

**Tech Stack:** Rust, orca-tools, orca-runtime (tool dispatch, approvals, rewind capture), orca-tui (change previews).

**Spec:** `docs/superpowers/specs/2026-11-27-apply-patch-design.md`

## Global Constraints

- Write Orca's own parser from the format described in the spec. Do not copy code from the Apache-2.0 Codex sources (`/Users/qingyun/Documents/GitHub/codex` exists locally; don't open its apply-patch crate).
- Paths resolve with `resolve_workspace_path`. Line endings follow edit's CRLF rule (the whole-file majority decides). Writes stay in place, as `edit` writes today, not atomic.
- The tool ships enabled only if the measurement in Task 3 shows no harm. Otherwise it ships behind `[tools] apply_patch = true`, off by default. Default for this branch: off, behind the flag.
- Commit after each task. Never push. No `TMPDIR`/`HOME` override. Never touch `~/.orca`. Use `env -u NODE_OPTIONS` for node/npm and workspace-wide cargo tests.

## Review Focus

- **A patch that adds and then updates the same path, or moves a file onto another file in the same patch.** Rejected in preflight as a repeated path. Test `a_path_used_twice_in_one_patch_is_rejected`.
- **Context lines that match only after whitespace is ignored.** Matching is tried exact first, then ignoring trailing whitespace, then ignoring surrounding whitespace. The first level that gives a unique match wins; an ambiguous match is an error. Test `context_matching_falls_back_through_whitespace_levels`.
- **An I/O failure on the third of five files.** The result lists two written, one failed and two not attempted, and the first two files are not rolled back. Test `a_commit_failure_reports_written_failed_and_not_attempted`.
- **A CRLF file updated by a patch written with LF.** The CRLF line endings are kept. Test `a_crlf_file_keeps_its_line_endings`.
- **A path outside the workspace in any operation.** The whole patch is rejected before anything is written. Test `an_outside_path_rejects_the_whole_patch`.

---

### Task 1: Parser and preflight

**Files:** Create `crates/orca-tools/src/apply_patch.rs`; register the module in `crates/orca-tools/src/lib.rs`.

**Interfaces:** Produces:
- `pub(crate) fn parse_patch(input: &str) -> Result<Patch, PatchError>`.
- `Patch { ops: Vec<PatchOp> }`, where `PatchOp` is `Add { path, lines }`, `Delete { path }` or `Update { path, move_to: Option<String>, hunks: Vec<Hunk> }`.
- `Hunk { context_hint: Option<String>, lines: Vec<HunkLine> }`, with `HunkLine` being `Context(String)`, `Remove(String)` or `Add(String)`.
- `pub(crate) fn preflight(patch: &Patch, cwd: &Path) -> Result<Plan, PatchError>`. It resolves paths, checks existence, repeats, move targets and every hunk's match, and computes each new file's text without writing.

**Grammar:**
- `*** Begin Patch` / `*** End Patch`;
- `*** Add File: <path>`, followed by `+` lines;
- `*** Delete File: <path>`;
- `*** Update File: <path>`, an optional `*** Move to: <path>`, then hunks. Each hunk starts with `@@`, optionally followed by a hint, and its lines start with ` `, `-` or `+`;
- `*** End of File` is allowed at the end of a hunk.

Leading and trailing blank lines around the envelope are tolerated.

- [ ] **Step 1: Write the failing tests**:
  - `parses_add_delete_update_and_move`
  - `rejects_a_patch_without_begin_or_end`
  - `a_path_used_twice_in_one_patch_is_rejected`
  - `an_outside_path_rejects_the_whole_patch`
  - `updating_a_missing_file_is_rejected`
  - `adding_an_existing_file_is_rejected`
  - `context_matching_falls_back_through_whitespace_levels`
  - `an_ambiguous_hunk_is_rejected`
  - `a_crlf_file_keeps_its_line_endings` (preflight output)
- [ ] **Step 2:** `cargo test -p orca-tools --lib apply_patch -- --test-threads=1` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same command → PASS.
- [ ] **Step 5:** Commit `feat(tools): parse and check an apply_patch envelope before writing`.

### Task 2: The tool, behind a flag

**Files:**
- Modify `crates/orca-tools/src/registry.rs`: register `apply_patch` next to `edit`, with the same capability set, action kind, renderer and exposure.
- Modify `crates/orca-core/src/config/…`: add `[tools] apply_patch: bool`, default `false`.
- Modify the runtime path that captures rewind pre-images for `edit` and `write_file`, so it captures every file a patch touches.
- Modify the TUI change preview to show one diff per file.

**Tool description:** It shows the envelope with one short example, and says that the whole patch is checked before anything is written and that a check failure writes nothing.

- [ ] **Step 1: Write the failing tests**:
  - `apply_patch_is_absent_unless_enabled`
  - `apply_patch_applies_a_multi_file_patch`
  - `a_commit_failure_reports_written_failed_and_not_attempted`
  - `apply_patch_needs_the_same_approval_as_edit` (suggest mode asks; plan mode denies)
  - `rewind_restores_every_file_a_patch_changed`
- [ ] **Step 2:** Run the new tests by name → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p orca-tools --lib -- --test-threads=1`, plus the named runtime tests → PASS. Then run `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`, `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`, `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --locked -j 4`.
- [ ] **Step 5:** Commit `feat(tools): an apply_patch tool for multi-file changes, off by default`.

### Task 3: Docs

- [ ] Document the tool and the `[tools] apply_patch` flag in the tools docs under `site/src/docs/md/en` and `zh` (find the page that lists tools), and in `docs/tools-comparison.md` (mark the gap closed, behind a flag).
- [ ] Run `env -u NODE_OPTIONS npm --prefix site ci && env -u NODE_OPTIONS npm --prefix site run build`.
- [ ] Commit `docs(tools): apply_patch`.
- [ ] Skip the live on/off measurement. Say in the report that it was skipped; the user decides on the default later.
