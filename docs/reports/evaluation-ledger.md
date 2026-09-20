# Evaluation ledger

One row per evaluation round. **The protocol matters more than the number**: two entries are
only comparable when the source commit, harness, model, attempts/concurrency and task set
agree. Every entry links to the run's committed summary (per-task outcomes) and to the human
report; raw trajectories, container logs and `jobs/` output stay out of git.

| date | source | tasks | settings | TB2 mean | L1 | report / summary |
|---|---|---|---|---|---|---|
| 2026-09-15 | `3aa4efdfa` (v0.4.31) | 89 | `-k 1 -n 3`, build-timeout ×2 | **0.494** (44/89) | — | [report](2026-09-15-terminal-bench-2-v0.4.31.md) |
| 2026-09-16 | `48fa0a458` (v0.4.31 + evidence fixes) | 88 (`qemu-startup` excluded) | `-k 1 -n 3`, build-timeout ×2 | **0.727** raw · **0.761** infra-corrected | — | [report](2026-09-16-terminal-bench-2-v0.4.31-fixed.md) |
| 2026-09-19 | `5970b40c` (v0.4.32, 26 merged fixes) | 88 (`qemu-startup` excluded) | `-k 1 -n 6` | **0.7955** (70/88) · scored-only 0.8434 | **24/24 suites green** | [report](2026-09-19-v0.4.32-evaluation.md) · [summary](2026-09-19-tb2-v0432-summary.json) |

Machine-readable form: [`evaluation-ledger.json`](evaluation-ledger.json).

## How a number is defined

- **TB2 mean** = Harbor's own aggregation: every scheduled trial counts, and a trial that
  never produced a reward (infrastructure failure, timeout kill) counts as 0. The
  scored-only mean (denominator = trials that produced a reward) is reported next to it
  because it separates "the agent failed the task" from "the run lost the trial".
- **L1** = the local suite sweep (`scripts/eval/sweep.sh`): protocol contracts, ACP, signals,
  trust races, workflow, sandbox, MCP, recovery, retention, plus the fault-injection battery.
  It needs no network or API key and takes ~30 minutes.
- **Infra corrections** are called out explicitly in each report rather than silently folded
  into the mean (e.g. the 5 environment failures in the 2026-09-19 run were re-run one at a
  time and all five passed).

## Reproducing a row

```bash
# L1 (no network, no API key)
bash scripts/eval/sweep.sh jobs/eval-sweep/$(date +%Y%m%d)

# TB2 (Harbor + Docker + API key), then turn the run into the committed summary
harbor run -d "terminal-bench/terminal-bench-2" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  -k 1 -n 6 --exclude-task-name "terminal-bench/qemu-startup" \
  --mounts '[{"type":"bind","source":"'"$PWD"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]'
python3 scripts/eval/summarize.py jobs/full-88-<version>/<timestamp> \
  --out docs/reports/<date>-tb2-<version>-summary.json --source-commit "$(git rev-parse --short HEAD)"
```

## Findings that left a test behind

Evaluation only pays off when a finding becomes a permanent check:

| finding | permanent check |
|---|---|
| #72 SIGINT/SIGTERM killed the run without a terminal record | `tests/signal_contract.rs`, `scripts/eval/signal_probe.py` |
| #69 `bwrap` never probed when `cwd` is an ancestor of `/usr/bin` | `crates/orca-tools` bwrap guard unit tests, `scripts/eval/sandbox_probe.py` |
| #81 concurrent `orca trust add/remove` lost decisions | `tests/jsonl_surface_differential.rs`, `scripts/eval/trust_race_probe.py` |
| #115 a `lifetime: workspace` service is killed at session end | `scripts/eval/workspace_lifetime_probe.py` (in the sweep) |
| #67 duplicate provider tool-call id aborted the session | `scripts/eval/fault_injection.py` (`duplicate_tool_call_id`), `model_response` unit tests |
