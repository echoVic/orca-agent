# grep passes its pattern and path to rg as values

Date: 2026-11-27. Size: small change. Approach chosen by the user: 1.

## Intent

The `grep` tool should treat the model's `pattern` and `path` as data, and search only inside the workspace, the same way `read_file`, `glob`, `list_files` and `read_document` already do.

## How it works today

- `crates/orca-tools/src/grep.rs` builds `rg --line-number --no-heading <pattern> <path>`.
  - A pattern that starts with `-` is read by rg as one of its own options. Ordinary searches such as `-> Result` fail with an rg usage error, and a pattern that names an rg option changes what rg does.
  - rg also reads the host's ripgrep config file (`RIPGREP_CONFIG_PATH`), so a personal rg config changes what the tool returns.
- `path` is joined to the cwd and only checked with `exists()`. An absolute path, or a `..` path, reaches outside the workspace. The in-process fallback, used when rg is not installed, joins the path the same way.
- Every other read tool resolves its path with `resolve_workspace_path` (`crates/orca-tools/src/lib.rs`).
- run_script's `tools.grep` goes through the same `grep::execute_or_cancel`, so it shares both behaviours.

## Goals

- The pattern always reaches rg as the pattern, and the path always as a path.
- rg ignores config files.
- grep resolves `path` with `resolve_workspace_path`. It refuses a path outside the workspace with that function's error (`path escapes workspace: …`), as the other read tools do. This also covers a symlink that leads out of the workspace.
- The in-process fallback follows the same path rule.

## Non-goals

- No change to output format, paging (`head_limit`, `offset`), timeouts or cancellation.
- No change to the tool's description or schema. Changing them would alter the cached prompt prefix for every session, and the error text already tells the model what went wrong.
- No sandbox for rg, and no replacement of rg (approaches 2 and 3 below).

## Approaches

1. **Chosen.** Keep rg. Pass `--no-config`, `--regexp=<pattern>`, and `--` before the path. Resolve the path with the workspace check on both the rg path and the in-process fallback. It is a small change, and ordinary searches behave the same.
2. **Dropped: search in-process only.** It would remove the subprocess, but it is slower on large repositories (rg searches in parallel) and could change binary-file and encoding handling. That is too large a behaviour change for a fix.
3. **Dropped: approach 1 plus rg inside the read-only sandbox.** grep would start failing on hosts where no sandbox can be enforced (for example a container without bwrap), and every call would pay the sandbox start-up cost. Once the arguments are values, the extra protection adds little.

## Design

- Command line: `rg --no-config --line-number --no-heading --regexp=<pattern> -- <path>`. The arguments are built by one small function, so a unit test can check them.
- `<path>` is the path `resolve_workspace_path` normalized, not the raw string. A relative input stays relative to the cwd (`.` when it normalizes to the cwd itself), so output paths keep today's form. An absolute input inside the workspace stays absolute.
- A path that does not exist still returns `(no matches)`, as today.
- The in-process fallback receives the same normalized path.

## Tests

Use neutral examples only:

- A pattern starting with `-` (`-> Result`) matches that literal text.
- A pattern that is an rg option name (`--files`) matches only lines containing that text; it does not list files.
- The built command line contains `--no-config`, `--regexp=`, and `--` before the path.
- `path` set to `..`, to an absolute directory outside the workspace, or to a symlink inside the workspace that points outside, is refused with `path escapes workspace`.
- The in-process fallback refuses the same paths and finds the same `-> Result` match.

## Constraints

- Code comments, commit messages, test names, the PR description and the release note describe the change by what it does for a user: patterns starting with `-` work, rg's config does not change results, and paths stay inside the workspace.
- Windows: `--regexp=` and `--` are rg options, not shell syntax, so they behave the same. Windows compiles only in CI.

## Release

v0.5.24, released the day the fix merges. The release note says that grep searches for the pattern exactly as given, even when it starts with `-`, and only inside the workspace.

## Left to the builder

- The helper's name and signature.
- How the fallback receives the resolved path.
