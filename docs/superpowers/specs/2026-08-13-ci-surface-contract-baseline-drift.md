# CI Fix: Runtime Surface Contract Baseline Drifted After TUI Relocation Slices

## Status

Implemented on `codex/tui-convergence`, based on `main` at `cab4fc5e7`.

## Problem And Evidence

CI has been red on main since `5efdc1607` (slice 2): both workflows
("Runtime Surface Contract" and "Windows") fail with
`unclassified associated TUI function item resume_terminal_render`.
Local repro: `node --test scripts/test-validate-runtime-surface-contract.mjs`
throws the same error on a clean main checkout.

Root cause: slices 2 and 5 relocated `resume_terminal_render` and
`clear_terminal_scrollback` out of `app.rs` (into `presentation.rs` and
`scrollback.rs`) without refreshing the reviewed inventories in
`scripts/validate-runtime-surface-contract.mjs`. The validator fail-fast
reports only the first drifted site; a full scan diff
(`scanTuiMutationSurface` vs the baseline maps) shows exactly two drifted
entries:

- `BASELINE_HARMLESS_ASSOCIATED_FUNCTION_ITEM_SITES`: the two sites moved
  to the new module paths (counts unchanged at 1).
- `BASELINE_HARMLESS_ASSOCIATED_FUNCTION_SHA256`:
  `resume_terminal_render` moved with an unchanged body hash
  (`8ff17eeb…`); `clear_terminal_scrollback` moved with a new reviewed
  body hash (`6a0f700c…`) because slice 5 hoisted its inner `use`
  imports to module scope.

Classification: local defect (reviewed inventory not refreshed in the
same commit as a relocation); dev-only CI gate, no runtime behavior
change.

## Expected Behavior

The validator accepts the relocated functions at their new module sites
with the reviewed counts and body hashes; both CI workflows return green.

## RED

Before the fix:
`node --test scripts/test-validate-runtime-surface-contract.mjs` fails
with the unclassified-site error (reproduced locally and in CI).

## Acceptance

1. Both validators pass from the fix commit:
   - `node --test scripts/test-validate-runtime-surface-contract.mjs`
   - `node --test scripts/test-validate-windows-platform-boundaries.mjs`
2. CI green for the fix commit on main ("Runtime Surface Contract" and
   "Windows" workflows).
3. No runtime source changed; cargo test suites unaffected.

## Compatibility

Dev-only validator baselines; no CLI/TUI/server/JSONL/persistence change.

## Process Fix

The convergence slice checklist now requires refreshing the
surface-contract baseline in the same commit as any relocation of a
function listed in the validator inventories (see the appended note in
`docs/superpowers/specs/2026-08-13-tui-convergence-insert-escape.md`).
