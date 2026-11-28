# Workspace Reads Stay in the Workspace

## Intent

Orca's read tools promise to stay inside the workspace. Today they check a path and then open it by name later, so a directory that is replaced by a link between the check and the open can send the read elsewhere. This change closes that window and four smaller gaps found in the same review.

## Goals

1. **Content reads.** `read_file`, `read_document`, `grep` and Workflow scripts never return content from outside the workspace. This holds even when a path component is swapped for a link during the call. Links inside the workspace that point inside it keep working.
2. **Listings.** `glob` and `list_files` never return an entry whose real location is outside the workspace.
3. **`rg`.**
   - Where Orca can enforce restricted reads, `rg` runs inside a read-only sandbox whose read roots are the workspace plus the two authorized ignore sources: macOS with Seatbelt enforced, and Linux with bwrap or Landlock.
   - Everywhere else, `grep` uses Orca's in-process search, which opens every file through the verified path: Windows, and Linux without a usable sandbox.
4. **Ignore files.**
   - In-tree `.gitignore` and `.ignore` files still apply, but a symlinked ignore file is skipped.
   - Two Git sources outside the workspace remain allowed: the user's global excludes file (`core.excludesFile`, default `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`) and the repository's `info/exclude`, including a linked worktree's common directory.
   - No other file outside the workspace is read.
   - This keeps today's results for ordinary repositories.
5. **Windows path spelling.**
   - A request for `C:\work\repo\src` is accepted when the workspace is spelled `\\?\C:\work\repo`, and the other way round. The same holds for UNC paths and their verbatim spelling, and for mixed separators.
   - Drive-relative forms such as `C:foo` are refused with a clear message.
6. **Existence semantics.**
   - A requested path keeps its meaning. `Cargo.toml/../crates` does not silently become `crates`, and a missing intermediate directory is an error, as before the grep change.
   - An empty `path` means the workspace root, explicitly.
7. **Workflow paths.**
   - `name` must be a single saved-workflow name, with no separators and no `.` or `..`.
   - When the model calls the Workflow tool, `scriptPath` must resolve inside the workspace or inside a workflows directory Orca already uses: a trusted ancestor's `.orca/workflows`, or the user workflow directory.
   - A path the user types in a command keeps working as today.

## Non-goals

- No change to which files the shell tools can read; that is the sandbox policy's job.
- No change to spill files, which live in Orca's session directory and are read through the existing spill path.
- No performance work beyond keeping `rg` where it can be confined.

## Decisions taken with the user (2026-11-28)

- **Ignore files:** keep the global excludes file and the repository's `info/exclude` as the only outside sources. Skip every other outside ignore file.
- **Hosts without a read sandbox:** use the in-process search. It may be slower than `rg` on very large repositories.

## Approaches considered

- **Open-then-verify by name.** Open by name, then check the handle's real path. This works for files, but it still lists the wrong directory if a directory is swapped. Rejected as the only mechanism.
- **Verified traversal plus a sandboxed `rg`** (chosen).
  - Resolve the canonical path and check containment.
  - Then open it component by component from the canonical workspace root with `orca_platform::fs::VerifiedDirectory`, which follows no links. If a component has become a link, the open fails.
  - `rg` cannot be confined from the outside, so the sandbox enforces its reads. Where there is no sandbox, Orca does not use it.
- **Drop `rg` everywhere.** Simplest, but it loses `rg`'s speed on the platforms that can confine it. Rejected.

## Left to the builder

- Where the verified-open helper lives: orca-tools or orca-platform.
- How the in-process search builds ignore matchers. The `ignore` crate's `Gitignore` builder fed with verified reads, or an equivalent.
- How `rg`'s read errors for denied files are reported. A denied read is a skipped file, not a failed search.
