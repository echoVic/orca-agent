# TUI Convergence Slice 1: Insert-Escape Flush Orchestration Extraction

## Status

Proposed for `codex/tui-convergence`, based on `main` at `54f9700fc`.

## Problem And Evidence

`crates/orca-tui/src/app.rs` is 10,165 lines; the roadmap's "TUI/runtime
protocol convergence" item is still open ("renderer-owned orchestration
and projection duplication remain"). The file mixes free-function
orchestration with the AppState impl and ~7,000 lines of tests. The
insert-escape flush policy (free functions at lines 170-232:
`refresh_after_insert_escape_flush`, `resolve_pending_insert_escape_before_routing`,
`flush_pending_insert_escape_before_non_key`, `flush_expired_insert_escape`)
is a coherent, bounded orchestration category — input-ownership
preprocessing for paste/submit/shortcut routing — with a strong existing
behavioral test suite (lines 1295-1566).

Classification: architecture boundary (module ownership), no behavior
change.

## Scope

Move the four free functions (and any small helpers they exclusively use)
into a new `crates/orca-tui/src/insert_escape.rs` module; `app.rs` imports
them. No logic edits.

## Non-Goals

- No AppState impl extraction, no projection changes, no protocol/CLI/
  persistence changes.
- No source-line-count assertions; the existing tests are the oracle.

## Ownership

`insert_escape.rs` owns the insert-escape flush policy; `app.rs` owns
routing and dispatch. Types stay in `types.rs`.

## Acceptance

1. The four functions live in insert_escape.rs; app.rs imports them;
   compile clean.
2. The existing behavioral tests (insert-escape preflight, paste/submit
   ownership, expired flush refresh) pass unchanged:
   `cargo test -p orca-tui --lib --locked` plus the PTY contract
   `cargo test -p orca-tui --test tui_pty_contract --locked -- --test-threads=1`.
3. `cargo fmt --all -- --check` and `git diff --check` clean.
4. Diff review: relocation only.

## Rollback

Single revertible commit; no persisted state.
