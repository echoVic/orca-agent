# An apply_patch tool in the patch format models already write

Date: 2026-11-27. Size: small project. It draws on DeepSeek's harness (dsh); the user asked for this learning to be adopted.

## Intent

A change across several files currently takes one `edit` or `write_file` call per change, and each call is a model round. Models trained on coding agents also write a well-known patch envelope fluently: `*** Begin Patch`, `*** Add File`, `*** Update File` with `@@` hunks, `*** Delete File`, `*** Move to`, `*** End Patch`.

dsh ships `apply_patch` with one string field, `input`, and has enabled it by default since 2026-10-29, after benchmark comparisons. It named the field `input` because 14% of calls in 2,226 DeepSeek rollouts used that name. It checks the whole patch before writing anything, and stops at the first failure without rolling back. Orca's own gap table lists apply_patch as P2 (`docs/tools-comparison.md`).

## Goals

1. **A new tool.** `apply_patch` takes `{"input": string}` with the envelope above and is registered beside `edit`. It has the same write capability, approval action, sandbox profile, rewind pre-image capture and workspace rule as `edit` and `write_file`.
2. **A preflight before any write.** Parse the whole patch and check every operation:
   - files to update or delete exist, and files to add do not;
   - move targets do not exist;
   - no path appears twice;
   - every path stays inside the workspace;
   - every hunk's context matches.

   Context matching works in this order, as the format's reference implementation does: exact lines first, then lines with trailing whitespace ignored, then lines with all surrounding whitespace ignored.

   Any failure writes nothing and returns which operation failed, and why.
3. **Commit in order.** Line endings follow the CRLF rule that `edit` uses. Writes stop at the first I/O failure, and the result lists what was written, what failed, and what was not attempted. There is no rollback, but rewind still covers the turn.
4. **The result** lists each file as added, updated, deleted or moved, with line counts, in the same shape `edit` reports a change. The TUI shows the per-file diffs as it does for `edit`.
5. **The description** shows the envelope with one short example and states the preflight rule.

## Non-goals

- No freeform or grammar-constrained tool API; a JSON string field is enough.
- No removal of `edit` or `write_file`.

## Constraints

- Implement it from the format described here, and write the parser in Orca. Do not copy code from the Apache-2.0 Codex sources.
- Adding a tool changes the tool list's bytes once, and the release note says so.
- Measure: run the existing comparison harness (`scripts/eval/code_mode/` style) on a few multi-file edit tasks with the tool off and on, and report rounds and cost. If it does not help, ship it off by default behind `[tools] apply_patch = true`.
