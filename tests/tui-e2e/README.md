# TUI end-to-end tests (`tui-test`)

Black-box PTY tests that drive a real `orca` binary with
[`tui-test`](https://github.com/microsoft/tui-test). They complement
`tests/tui_pty_contract.rs` (in-process TUI contract with a mock runtime) by
exercising the shipped binary: first-run welcome, composer keys, approval modes,
slash commands, mentions, resize, and a live model round trip.

These are not cargo tests; `cargo test` does not run them.

## Requirements

- A built or installed `orca` on `PATH`, or `ORCA_BIN=/path/to/orca`.
- `tui-test` on `PATH` (stable 0.1.x is enough):

  ```sh
  brew trust microsoft/tui-test
  brew install tui-test
  ```

  On Linux/macOS without Homebrew, use the install script from the
  [tui-test repo](https://github.com/microsoft/tui-test). The suite uses the
  stable 0.1.x CLI (`expect text`, `expect mode`, `expect title`, `get clipboard`),
  not the 2.x beta rewrite.

## Run

```sh
cargo build --workspace            # or: npm install -g @blade-ai/orca
ORCA_BIN=target/debug/orca tests/tui-e2e/orca-smoke.sh      # 21 checks
ORCA_BIN=target/debug/orca tests/tui-e2e/orca-extended.sh   # 41 checks
LIVE=1 ORCA_BIN=target/debug/orca tests/tui-e2e/orca-extended.sh  # +6 live checks
```

Both scripts exit non-zero on failure, print one `PASS`/`FAIL` line per check,
and write screenshots to `tests/tui-e2e/shots/` (git-ignored).

`LIVE=1` sends real prompts and needs a configured `DEEPSEEK_API_KEY` (or the
auth file the binary already uses); the default suites make no network calls.

## Coverage

`orca-smoke.sh` (21): welcome/trust screen, continue-untrusted, splash, `?` help,
`/` palette, `/status` expansion, resize, double `Ctrl+C` quit.

`orca-extended.sh` (41, +6 with `LIVE=1`):

- welcome T/U/E, `Esc` exits, `--plan` starts in plan mode
- help keys (`?` only on empty composer, `Ctrl+K`, `F1`), `Esc`/draft semantics
- approval cycle: full-auto → plan → suggest → auto-edit → Full Access modal
- `Ctrl+L`, `Ctrl+U`, `Ctrl+C` arming quit
- `/model`, `/base-url`, `/resume`, `/trust`, `/side`
- `@` mentions (files + skills, trusted and untrusted), `$` skills
- trusted vs untrusted welcome, unicode input, 40x12 layout
- live: prompt/reply, `/copy` clipboard, `/status` title+usage, `Esc` backtrack,
  `Ctrl+C` cancels a running turn

## Notes

- Every run uses throwaway workspaces under `/tmp` and never touches your
  checkout. Trust for the temporary workspace is removed after the run.
- Orca records a per-workspace welcome acknowledgement (with a security policy
  digest) in `~/.orca/onboarding.toml`; there is no CLI to prune those entries,
  so long local runs accumulate a few.
- Screenshots render the splash's braille art as tofu when the render font lacks
  braille glyphs; text assertions are unaffected.
- The tests are timing-sensitive by nature (PTY + TUI). If a check flakes,
  re-run once before investigating; the stable tui-test `expect` commands retry
  until their timeout.
