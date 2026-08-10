# Obsolete Worktree Cleanup

## Scope

After the seven roadmap slices, only these five clean worktrees/branches are
cleanup targets:

```text
.worktrees/auto-memory-governance   codex/auto-memory-governance
.worktrees/headless-trajectory-truth codex/headless-trajectory-truth
.worktrees/mcp-sse-elicitation      codex/mcp-sse-elicitation
.worktrees/network-ask-on-block     codex/network-ask-on-block
.worktrees/side-conversation        feat/side-conversation
```

## Provenance

`git cherry main` marks the unique commits on the first four branches as
already represented by equivalent patches on `main`. The side-conversation
branch is an older sibling implementation (`d088d702e`); `main` contains the
replacement `8a7ae4584` plus the subsequent shutdown-boundary, contract, PTY,
and transcript fixes through `e23b7f86c`. All five worktrees were clean before
removal. No unrelated worktree is in this list.

## Preserved work

The following linked worktrees and branches remain registered because they are
not cleanup targets: `integrate-reliability-slices`, `mcp-wire-elicitation`,
`tui-terminal-wait-cancellation`, the issue-28 checkouts, and all six roadmap
slice worktrees until their commits are integrated.

## Verification boundary

Cleanup is limited to `git worktree remove` for the five exact paths followed
by deletion of their exact branch refs. No reset, broad recursive deletion, or
remote operation is used.
