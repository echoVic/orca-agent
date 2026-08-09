# Windows Release Gate Repair Implementation Plan

**Goal:** Restore the native Windows release gate without weakening memory
cancellation or headless trajectory assertions.

## Task 1: Preserve existing-memory rollback on Windows

- [x] Keep the existing RED GitHub failure as the platform reproduction.
- [x] Open the append handle with truncation-capable write access.
- [x] Run the focused memory cancellation test.

## Task 2: Bound the long headless contract explicitly

- [x] Add a nextest override for only
  `headless_max_inner_turns_preserve_trajectory_truth`.
- [x] Keep the 128-turn, terminal projection, and persisted transcript
  assertions unchanged.
- [x] Run the exact focused headless contract.

## Task 3: Qualify and integrate

- [x] Run formatter, diff check, focused package tests, and the full workspace
  gate.
- [x] Review the complete diff and commit one semantic repair.
- [x] Fast-forward the repair into `main`, push, and require fresh Runtime
  Surface, Pages, and native Windows success before tagging.
