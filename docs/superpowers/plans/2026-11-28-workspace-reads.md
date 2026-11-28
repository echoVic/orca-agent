# Workspace Reads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read tools never return content or entries from outside the workspace, even when a path changes during the call. Also fix the ignore-file, Windows path, existence and Workflow path gaps.

**Architecture:**
- **Content reads** go through one verified-open helper:
  1. Canonicalize the path and check containment.
  2. Open it from the canonical workspace root with `orca_platform::fs::VerifiedDirectory`, which follows no links.
  3. Refuse if any component became a link.
- **Listings** check each entry the same way before returning it.
- **`rg`** runs only inside an enforced read-only sandbox. Otherwise `grep` uses the in-process search.

**Tech Stack:** Rust. In orca-tools: `lib.rs` (`resolve_workspace_path`), `read_file.rs`, `read_document.rs`, `grep.rs`, `glob.rs`, `list_files.rs`, `process.rs` and `sandbox/`. Also orca-platform `fs/directory.rs`, and orca-runtime `workflow/script.rs`.

**Spec:** `docs/superpowers/specs/2026-11-28-workspace-reads-design.md`

## Global Constraints

**Messages**
- A path changed during the open: `path changed while it was opened: <path>`.
- A drive-relative Windows path: `drive-relative paths are not supported: <path>`.
- A Workflow `name` with path parts: `workflow name must be a saved workflow name, not a path: <name>`.
- A model-supplied `scriptPath` outside the allowed roots: `scriptPath must be inside the workspace or a workflows directory: <path>`.

**Behaviour that must not change**
- Links inside the workspace that point inside it keep working for every read tool.
- Ordinary repositories get the same `grep` results as today, including the global excludes file and `info/exclude`.
- An empty `path` means the workspace root.
- `rg` keeps `--no-config`, `--regexp=<pattern>` and `--` before the path.

**Process**
- Commit after each task. Never push.
- Do not override `TMPDIR` or `HOME`, and never touch `~/.orca`. Tests that need a global excludes file pass its path explicitly.
- Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo test runs.
- Symlink tests are `#[cfg(unix)]`. Windows-only path tests are `#[cfg(windows)]` and run in CI.
- The string-normalization logic gets portable unit tests that run on every host.

## Review Focus

- **A directory swapped for an outward link after resolution.** Every content read refuses with the changed-path message and returns no outside bytes. Test: `a_directory_swapped_for_a_link_after_resolution_is_refused`, driving the helper between its resolve and open steps.
- **A symlinked `.gitignore` pointing outside.** The in-process search does not read its rules, and `rg` cannot read it either. Files the outside rules would hide are still searched. Test: `a_symlinked_ignore_file_pointing_outside_is_not_read`.
- **`rg` reaching outside its read roots.** It returns nothing from outside, and a denied file counts as skipped, not as a failed search. Test: `sandboxed_rg_returns_nothing_outside_its_read_roots`. It is skipped when enforcement is unavailable, like the other Seatbelt and Landlock tests.
- **Verbatim and normal Windows spellings.** These are accepted both ways, and `C:foo` is refused. Tests: `verbatim_and_normal_windows_paths_compare_equal` (portable, on the normalizer) and the `#[cfg(windows)]` cases.
- **The model passing `scriptPath: "../x.js"` or `name: "../x"`.** Both are refused, and a path the user types still works. Tests: `a_model_script_path_outside_the_workspace_is_refused`, `a_workflow_name_with_path_parts_is_refused`, `a_user_typed_script_path_still_runs`.

---

### Task 1: Verified opens for content reads

- [ ] **Step 1: Write the failing tests.**
  - `a_directory_swapped_for_a_link_after_resolution_is_refused`
  - `in_workspace_links_still_read`, covering `read_file`, `read_document` and the in-process `grep`
  - `an_outward_link_in_a_parent_is_refused`
- [ ] **Step 2: Implement.** Add a helper that returns an open `File` for a path inside the workspace using `VerifiedDirectory`. Use it in `read_file` (not for spill files), `read_document` and the in-process `grep` per-file opens.
- [ ] **Step 3:** Run `cargo test -p orca-tools --lib -- --test-threads=1` and `cargo test -p orca-platform`, and confirm they pass.
- [ ] **Step 4: Commit** `fix(tools): a file read stays in the workspace even if a folder is replaced by a link`.

### Task 2: Listings never return outside entries

- [ ] **Step 1: Write the failing tests.**
  - `glob_skips_entries_whose_real_location_is_outside`
  - `list_files_skips_entries_whose_real_location_is_outside`

  Each test calls the entry check directly with a parent that is a link to outside.
- [ ] **Step 2: Implement** the check in `glob.rs` and `list_files.rs`.
- [ ] **Step 3: Run the tests and commit** `fix(tools): glob and list_files only return entries inside the workspace`.

### Task 3: `rg` runs only inside an enforced read sandbox

- [ ] **Step 1: Write the failing tests.**
  - `sandboxed_rg_returns_nothing_outside_its_read_roots`
  - `grep_uses_the_in_process_search_without_enforced_reads`, through an injectable enforcement state
  - `a_denied_file_is_skipped_not_a_failed_search`
- [ ] **Step 2: Implement.**
  - Spawn `rg` through the execution broker. The `CapabilityRequest` has read roots for the canonical workspace and the existing authorized ignore files, and no writes and no network.
  - Use it only when the broker reports `EnforcementState::Enforced` for restricted reads on this host. Otherwise run the in-process search.
  - Keep the existing argv.
- [ ] **Step 3: Run the tests and commit** `fix(tools): grep runs rg only where its reads can be kept in the workspace`.

### Task 4: Ignore rules without outside reads

- [ ] **Step 1: Write the failing tests.**
  - `a_symlinked_ignore_file_pointing_outside_is_not_read`
  - `the_global_excludes_file_still_filters`, with the path passed explicitly
  - `a_linked_worktrees_info_exclude_still_filters`
  - `in_tree_gitignore_still_filters`
- [ ] **Step 2: Implement.**
  - In-tree ignore files are read only through verified opens.
  - The global excludes file and `info/exclude` are resolved once and read explicitly. They are also the only outside read roots given to `rg`.
- [ ] **Step 3: Run the tests and commit** `fix(tools): grep reads in-tree ignore files without following links`.

### Task 5: Windows path spelling

- [ ] **Step 1: Write the failing tests.**
  - `verbatim_and_normal_windows_paths_compare_equal`, portable, on the normalizer
  - `verbatim_unc_and_unc_compare_equal`
  - `drive_relative_paths_are_refused`
  - `#[cfg(windows)]` resolver tests for each spelling and for mixed separators
- [ ] **Step 2: Implement** the comparison in `resolve_workspace_path`, using `orca_core` `workspace_identity` where it fits.
- [ ] **Step 3: Run the tests and commit** `fix(tools): Windows paths are accepted however the workspace is spelled`.

### Task 6: Existence semantics

- [ ] **Step 1: Write the failing tests.**
  - `a_file_used_as_a_directory_is_an_error`, for `Cargo.toml/../crates`
  - `a_missing_intermediate_directory_is_an_error`
  - `an_empty_path_means_the_workspace_root`
  - `a_dot_slash_prefix_keeps_relative_output`
- [ ] **Step 2: Implement** in `resolve_workspace_path`. Keep containment checked before any filesystem access.
- [ ] **Step 3: Run `cargo test -p orca-tools --lib -- --test-threads=1` and commit** `fix(tools): a requested path keeps its meaning`.

### Task 7: Workflow names and script paths

- [ ] **Step 1: Write the failing tests.**
  - `a_workflow_name_with_path_parts_is_refused`
  - `a_model_script_path_outside_the_workspace_is_refused`
  - `a_model_script_path_in_a_workflows_directory_runs`
  - `a_user_typed_script_path_still_runs`
  - `a_script_path_swapped_for_a_link_is_refused`
- [ ] **Step 2: Implement** in `crates/orca-runtime/src/workflow/script.rs` and at the model-facing call site, not the user command paths. Read the script through the Task 1 helper.
- [ ] **Step 3:** Run `env -u NODE_OPTIONS cargo test -p orca-runtime --lib workflow -- --test-threads=1`.
- [ ] **Step 4: Commit** `fix(workflow): the Workflow tool runs saved names and scripts from the workspace or workflow folders`.

### Task 8: Docs and checks

- [ ] **Docs.** Update the tools docs and the Workflow docs in English and Chinese: the site under `site/src/docs/md/{en,zh}` and `docs/`. Cover what the read tools refuse, the two outside ignore sources, and where `grep` uses the in-process search.
- [ ] **Release note.** Add an item to `docs/releases/unreleased.md`, creating it if needed.
- [ ] **Workspace checks.** Run:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -j 4`
  - `env -u NODE_OPTIONS cargo test --workspace --all-targets --no-fail-fast -- --test-threads=1`
  - the runtime-surface validator and its test script
  - the Windows platform-boundary validator and its test script
  - the execution-broker and HTTP-client boundary validators
  - the site build and SEO check
  - `cargo check --target x86_64-pc-windows-msvc --locked --all-targets -p orca-core -p orca-platform`
- [ ] **Commit** `docs: what the read tools keep inside the workspace`.
