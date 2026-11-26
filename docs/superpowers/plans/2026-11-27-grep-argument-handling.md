# grep Argument Handling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `grep` passes the model's pattern and path to `rg` as values, ignores rg config files, and searches only inside the workspace; then v0.5.24 is prepared.

**Architecture:** One function builds rg's argument list. The path is resolved once with `resolve_workspace_path` before either the rg path or the in-process fallback runs. Release preparation follows `docs/release-process.md` steps 2, 3 and 5.

**Tech Stack:** Rust (orca-tools), ripgrep as a subprocess, the `regex` and `ignore` crates for the fallback; the site is Vite/TypeScript.

**Spec:** `docs/superpowers/specs/2026-11-27-grep-argument-handling-design.md`

## Global Constraints

- Code comments, test names, commit messages and release text describe the change by what it does for a user.
- Do not change the grep tool's description or JSON schema in `crates/orca-tools/src/registry.rs`.
- Output format, paging (`head_limit`, `offset`), the 120 s timeout and cancellation stay as they are.
- Commit after each task; never push.
- Do not set or override `TMPDIR` or `HOME`, and never read or write `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.
- This worktree uses its own `target/` (no `CARGO_TARGET_DIR`), so the first build is cold.

## Review Focus

- A path that normalizes to the workspace itself (`.`, `./`, `src/..`) reaches rg as `.`, never as an empty argument. Task 2 test `a_path_naming_the_workspace_itself_searches_everything`.
- A pattern containing `=` (`a=b`) survives the `--regexp=` form intact. Task 1 test `a_pattern_with_an_equals_sign_is_searched_literally`.
- An absolute path inside the workspace keeps absolute output paths, as today. Task 2 test `an_absolute_path_inside_the_workspace_is_searched`.
- Windows separators: a relative input such as `src` reaches rg relative to the cwd, and `resolve_workspace_path` behaves exactly as it does for `read_file`. There is no extra Windows test; CI's Windows jobs run the Task 1 and 2 tests.
- Empty pattern `""` still matches every line, as `rg ""` does today. Task 1 test `an_empty_pattern_still_matches_every_line`.

---

### Task 1: rg receives the pattern and path as values

**Files:**
- Modify: `crates/orca-tools/src/grep.rs` (the `Command::new("rg")` block in `execute_or_cancel`, and the tests module)

**Interfaces:**
- Produces: `fn rg_args(pattern: &str, search_path: &Path) -> Vec<std::ffi::OsString>` in `grep.rs`, returning exactly `["--no-config", "--line-number", "--no-heading", "--regexp=<pattern>", "--", <search_path>]`.

- [ ] **Step 1: Write the failing tests** in the `tests` module of `grep.rs`, using the module's existing `temp_dir` helper and `ToolRequest` shape:
  - `rg_args_pass_the_pattern_and_path_as_values`: `rg_args("-> Result", Path::new("src"))` equals `["--no-config", "--line-number", "--no-heading", "--regexp=-> Result", "--", "src"]` (as `OsString`s).
  - `a_pattern_starting_with_a_dash_is_searched_literally`: the workspace has `lib.rs` containing `fn load() -> Result<()> {` and `fn other() {}`. grep `{"pattern":"-> Result","path":"."}` completes, and its output has exactly one line, which contains `fn load() -> Result<()> {`.
  - `a_pattern_naming_an_rg_option_is_searched_literally`: the workspace has `notes.txt` with the line `pass --files to list them` and `other.txt` with `nothing here`. grep `{"pattern":"--files"}` completes with exactly one output line, which contains `pass --files to list them`. No output line is a bare file name such as `other.txt`.
  - `a_pattern_with_an_equals_sign_is_searched_literally`: the file holds `a=b` and `ab`. Pattern `a=b` gives exactly one line, containing `a=b`.
  - `an_empty_pattern_still_matches_every_line`: a two-line file; pattern `""` gives both lines.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-tools --lib grep::tests -- --test-threads=1`
Expected: FAIL. `rg_args` does not exist; once it compiles, the dash and option-name tests fail with an rg usage error.

- [ ] **Step 3: Implement `rg_args` and build the rg command from it** in `execute_or_cancel`: `Command::new("rg").args(rg_args(pattern, &search_path))`, keeping `current_dir(cwd)`, the piped stdio and `prepare_non_interactive_command`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p orca-tools --lib grep::tests -- --test-threads=1`
Expected: PASS, including the existing grep tests.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tools/src/grep.rs
git commit -m "fix(tools): grep passes the pattern and path to rg as values"
```

### Task 2: grep searches only inside the workspace

**Files:**
- Modify: `crates/orca-tools/src/grep.rs` (`execute_or_cancel`, `execute_in_process`, tests)

**Interfaces:**
- Consumes: `rg_args` from Task 1; `crate::resolve_workspace_path(cwd: &Path, target: Option<&str>) -> Result<PathBuf, String>` from `crates/orca-tools/src/lib.rs`.
- Produces: `execute_in_process(request: &ToolRequest, cwd: &Path, pattern: &str, search_path: &Path, max_bytes: usize, should_cancel: &dyn Fn() -> bool) -> ToolResult`. `search_path` is the resolved path; the function no longer joins a raw string.

- [ ] **Step 1: Write the failing tests:**
  - `a_path_outside_the_workspace_is_refused`: with `path` set to `..`, and with `path` set to the absolute path of a second temp directory holding a matching file, the result is `ToolStatus::Failed`, its error contains `path escapes workspace`, and the output never contains the second directory's match.
  - `a_symlink_leading_out_of_the_workspace_is_refused` (`#[cfg(unix)]`): `link` inside the workspace points to an outside temp directory holding a match. grep with `path: "link"` fails, and its error contains `path escapes workspace`.
    - This test calls `std::os::unix::fs::symlink`. The Windows platform validator counts every `std::os::unix::` occurrence per file, test code included.
    - Add the row `["tools-grep-test-unix-fixture", "crates/orca-tools/src/grep.rs", "unix_std_api", <occurrences in grep.rs>, "windows-test-portability"]` to `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json`. Put it next to the existing `tools-test-unix-fixture` row for `crates/orca-tools/src/lib.rs`, in the same commit.
  - `a_path_naming_the_workspace_itself_searches_everything`: for each of `.`, `./` and `src/..`, a match in `notes.txt` at the workspace root is found.
  - `an_absolute_path_inside_the_workspace_is_searched`: `path` set to the absolute path of `<workspace>/src` finds the match in `src/a.rs`, and the output line contains `a.rs`.
  - `the_fallback_searches_the_resolved_path_literally`: call `execute_in_process` with the resolved workspace path and pattern `-> Result`. It finds the one matching line (the fallback's regex treats `-` literally).
  - The existing `missing_search_path_completes_with_no_matches` still returns `(no matches)`. Keep it unchanged.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-tools --lib grep::tests -- --test-threads=1`
Expected: FAIL. Outside paths are searched today, and `execute_in_process` takes `&str`.

- [ ] **Step 3: Resolve the path first, then run rg or the fallback.**
  - `resolve_workspace_path(cwd, Some(&search_path))` → on `Err(message)` return `ToolResult::failed(request, message, None)`.
  - Keep today's `exists()` check on the resolved path, returning `(no matches)`.
  - Build the path rg receives:
    - For a relative input, strip the cwd prefix from the resolved path, using `.` when nothing is left.
    - For an absolute input, use the resolved path as it is.
  - Pass the same path to `execute_in_process`, which walks it directly.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p orca-tools --lib grep::tests -- --test-threads=1`
Expected: PASS.

Then run: `cargo test -p orca-tools --lib -- --test-threads=1`
Expected: PASS. This includes the run_script grep path through the registry.

Then run:

```bash
env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs
env -u NODE_OPTIONS node scripts/test-validate-windows-platform-boundaries.mjs
env -u NODE_OPTIONS node scripts/validate-execution-broker-boundary.mjs
cargo fmt --all -- --check
cargo clippy -p orca-tools --all-targets --locked
```

Expected: every command exits 0, and clippy prints no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tools/src/grep.rs docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json
git commit -m "fix(tools): grep keeps its search inside the workspace"
```

### Task 3: Prepare v0.5.24

**Files:**
- Modify: `Cargo.toml` (workspace `version = "0.5.24"`), `Cargo.lock` (via `cargo check`), `npm/orca/package.json` (`"version": "0.5.24"`)
- Create: `docs/releases/v0.5.24.md`
- Modify: `site/src/shared.ts` (`releaseVersion = "v0.5.24"`, new first entry in `releases` dated `2026-11-27` with url `https://github.com/echoVic/orca-agent/releases/tag/v0.5.24`), `site/src/changelog/Changelog.tsx` (a `"v0.5.24"` entry at the top of both the English and the Chinese `summaries`), `site/public/sitemap.xml` if the site build changes it.

**Interfaces:**
- Consumes: the behaviour from Tasks 1 and 2.

- [ ] **Step 1: Bump the three version fields.** Run `cargo check --locked -p orca-tools`, or `cargo check` if `--locked` refuses the bump, so that `Cargo.lock` follows.
- [ ] **Step 2: Write `docs/releases/v0.5.24.md`** in the format of `docs/releases/v0.5.23.md`, with these sections: summary sentence, Fixes, Compatibility, Verification, Upgrade.
  - The Fixes entry says three things:
    - grep searches for the pattern exactly as given, even when it starts with `-`. `-> Result` used to fail with an rg usage error.
    - rg's own config file no longer changes results.
    - A `path` outside the workspace, or a symlink leading out of it, is refused with `path escapes workspace`, as `read_file`, `glob` and `list_files` already do.
  - Compatibility says that a grep call that used to search outside the workspace now fails, and that the tool's description and schema are unchanged.
  - Verification lists the same commands as v0.5.23's.
  - Upgrade uses `@blade-ai/orca@0.5.24`.
- [ ] **Step 3: Update the site.** Edit `shared.ts` and both `summaries` in `Changelog.tsx`. The Chinese summary has the same content as the English one.
- [ ] **Step 4: Verify**

Run:

```bash
env -u NODE_OPTIONS node scripts/release/verify-version-sync.mjs
env -u NODE_OPTIONS npm --prefix site run build
env -u NODE_OPTIONS npm --prefix site run check:seo
```

Expected: all three exit 0.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock npm/orca/package.json docs/releases/v0.5.24.md site/src/shared.ts site/src/changelog/Changelog.tsx site/public/sitemap.xml
git commit -m "release: prepare v0.5.24"
```
