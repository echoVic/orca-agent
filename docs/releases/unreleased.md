# Unreleased

## Fixes

- `grep` searches in process on every platform, with native path ordering and continuation offsets that resume at the first match omitted by the output limit. It uses byte regexes with Unicode enabled by default, handles UTF-8 and UTF-16 BOMs and CRLF lines, and includes the file name when searching a single file. Recursive searches skip binary and unreadable files. Explicit file searches report binary matches and read errors. Lines over 16 MiB are skipped and counted. The search honors ignore files through the Git root, skips `.git`, and applies the [documented ignore rules](../harness-contract.md). A stalled read can outlive the 120-second search timeout. The shorter tool description causes one prompt-cache miss per session after upgrading.
- Orca fixes the workspace path when a session accepts it. Reads refuse a workspace or directory replaced by a link or junction. A re-clone at the same path works. `glob` and `list_files` recheck entries before returning them. Hard links inside the workspace are treated as ordinary files.
- Workflow drafts read their metadata and script from the session directory and refuse invalid draft IDs. The Workflow host receives the prepared script over stdin. Resuming reports an unreadable agent cache before starting a fresh agent.
