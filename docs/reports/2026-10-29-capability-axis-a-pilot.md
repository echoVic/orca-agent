# Capability axis A, pilot 1 — v0.5.14 failure set at 2× budget

Date: 2026-10-29 · Source: `5b3a7f1c` (v0.5.14) · binary: musl `x86_64-unknown-linux-musl`, sha256 `430fffce…`
Harness: Harbor 0.20.0, adapter `terminal_bench.orca_agent:OrcaInstalledAgent`, model `deepseek-flash`
Protocol: `-k 1 -n 4`, `--timeout-multiplier 2.0`, `--agent-timeout-multiplier 2.0`, one job over the 16
tasks that scored 0.0 in `jobs/full-88-v0514/2026-10-28__09-12-44`.

## Question

Of the 16 Terminal-Bench tasks the agent failed at the normal budget, how many are **time-bound**
(pass with a bigger wall-clock budget) and how many are **capability gaps** (fail anyway)? This is the
first measurement of the design's "time/throughput ceiling vs capability ceiling" split
(`docs/superpowers/specs/2026-10-29-capability-evaluation-design.md`, axes A).

## Result: 9 of 16 are time-bound (56 %)

| task | 1× (v0.5.14) | 2× |
|---|---|---|
| `chess-best-move` | 0.0 | **1.0** |
| `crack-7z-hash` | 0.0 (`AgentTimeoutError`) | **1.0** |
| `gpt2-codegolf` | 0.0 | **1.0** |
| `large-scale-text-editing` | 0.0 | **1.0** |
| `make-mips-interpreter` | 0.0 (114 turns) | **1.0** |
| `merge-diff-arc-agi-task` | 0.0 | **1.0** |
| `protein-assembly` | 0.0 | **1.0** |
| `pypi-server` | 0.0 | **1.0** |
| `torch-tensor-parallelism` | 0.0 | **1.0** |
| `adaptive-rejection-sampler` | 0.0 | 0.0 |
| `caffe-cifar-10` | 0.0 (`AgentTimeoutError`) | 0.0 |
| `configure-git-webserver` | 0.0 | 0.0 |
| `filter-js-from-html` | 0.0 | 0.0 |
| `make-doom-for-mips` | 0.0 (`AgentTimeoutError`) | 0.0 |
| `pytorch-model-cli` | 0.0 | 0.0 |
| `qemu-alpine-ssh` | 0.0 | 0.0 |

Transitions: `jobs/capability/A-20261029-011225/transitions.json` (run directory not committed; rows are
this table). One row of the first pass was Harbor's job-level aggregate and is now skipped by
`axes/axis_a.py`.

## What this changes

1. **0.8182 is substantially a clock artifact, not a capability ceiling.** Counting the nine 2× passes,
   the same run would be 81/88 = **0.9205**. The remaining gap at the normal budget is dominated by
   wall-clock, so the first lever is completion discipline / budget policy, not model capability.
2. **`pypi-server` was time-bound, not a service-lifetime bug.** The earlier suspicion (a
   `lifetime: workspace` service not surviving to the verifier) is wrong for this task: at 2× the same
   configuration scores 1.0. No issue is filed.
3. **`make-mips-interpreter` passes at 2×.** After the #114 adapter fix (stdin prompt) the task is
   solvable; at 1× the 114-turn run simply runs out of clock.
4. **Seven tasks still fail at 2×** and are queued for the 4× rung: `adaptive-rejection-sampler`,
   `caffe-cifar-10`, `configure-git-webserver`, `filter-js-from-html`, `make-doom-for-mips`,
   `pytorch-model-cli`, `qemu-alpine-ssh`. If several pass at 4×, the "capability gap" set shrinks
   further and the ceiling moves again.

## Follow-up

- 4× on the seven remaining tasks (running; same protocol with `--timeout-multiplier 4`).
- Fold both rungs into the capability scoreboard (`docs/reports/capability-scoreboard.json`) once the
  4× batch finishes; axis A then reports a two-point curve per task.
- P1 (axes B/C/D) stays blocked on the API-spend question; axis A cost is bounded and already spent.
