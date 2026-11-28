# Workspace Reads Implementation Plan

[Design](../specs/2026-11-28-workspace-reads-design.md)

The first pass added verified content opens, workspace-aware listings and Workflow paths, existence checks, and native Windows path spelling. It also introduced a restricted ripgrep launch and limited outside ignore sources. The November 28 review and decisions D1 and D2 replace that search and restore Git ancestor rules.

## Review rework

1. **Root pinning (1).** Drive a workspace replacement through `read_file` before resolution, then use a shared root identity check for verified content and model Workflow reads. Cover the configured user workflow directory and both link and ordinary workspace replacements.
2. **Workflow drafts (2).** Test draft ID validation, a linked draft metadata or script file, and a script replacement after preparation through `WorkflowDraftAction`. Verify draft files from the session root and check a digest at host startup.
3. **Listings (3).** Replace paths between enumeration and emission through the tools. Check leaf types relative to verified parent handles and recheck the selected result paths before emitting them.
4. **Ignore rules (D2, 5, 9).** Test ancestor `.ignore` and first-line BOM handling, linked-worktree backlinks, separate Git directories, and native macOS root identity. Load nearest repository ancestor rules through verified opens and check each outside Git directory before reading `info/exclude`.
5. **Windows (10).** Run both resolver tests and the native junction, spill, and enumeration cases explicitly in CI. Read spill files and enumerate directory entries through verified handles.
6. **Search (D1, 4, 6, 7, 8).** Remove the helper process and output parser. Walk verified directories in parallel, prune before opening children, and reuse parent handles when scanning files in path order. Test BOMs, binary and unreadable files, native filenames, paging, cancellation, and option-shaped patterns through `grep`. Update the description, check the prompt and serialized tool definitions, and compare release builds against `a497ab6f` on the same workspace with common and rare patterns.
7. **Docs and checks.** Update both languages and the release note. Run format, Clippy, the full workspace tests without ignored tests, each validator and its test script, site build and SEO, and the Windows cross-target check for `orca-core` and `orca-platform`.

Keep the current branch, commit each group, and run new regressions before and after the change. Keep check logs and benchmark builds under the ignored `target/` directory in this worktree.
