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

## 4× rung: one more time-bound, one environment, five capability

The seven tasks that failed at 2× were re-run with `--timeout-multiplier 4` (same protocol).

| task | 2× | 4× | reading |
|---|---|---|---|
| `caffe-cifar-10` | 0.0 | **1.0** | time-bound at 4× |
| `adaptive-rejection-sampler` | 0.0 | 0.0 | capability (statistics correctness) |
| `configure-git-webserver` | 0.0 | 0.0 | capability (server output assertion) |
| `filter-js-from-html` | 0.0 | 0.0 | capability (`test_clean_html_unchanged`) |
| `make-doom-for-mips` | 0.0 | 0.0 | capability (`test_vm_execution`) |
| `pytorch-model-cli` | 0.0 | 0.0 | capability (`test_cli_tool_output`) |
| `qemu-alpine-ssh` | 0.0 | 0.0 | **environment** — the verifier's `apt-get install` gets 404 from
  `deb.debian.org/debian-security bullseye/main` (pruned packages), so `curl`/`uvx` are missing and
  `/tests/test.sh` cannot run; the agent's work is never evaluated |

Transitions: `jobs/capability/A-20261029-021535/transitions.json` (not committed).

## Final split of the 16 v0.5.14 failures

| class | count | tasks |
|---|---|---|
| time-bound at 2× | 9 | chess-best-move, crack-7z-hash, gpt2-codegolf, large-scale-text-editing, make-mips-interpreter, merge-diff-arc-agi-task, protein-assembly, pypi-server, torch-tensor-parallelism |
| time-bound at 4× | 1 | caffe-cifar-10 |
| environment | 1 | qemu-alpine-ssh (verifier dependency 404, not the agent) |
| capability | 5 | adaptive-rejection-sampler, configure-git-webserver, filter-js-from-html, make-doom-for-mips, pytorch-model-cli |

So the same run read as **82/88 = 0.9318** at the 4× budget, and the honest capability gap on this
task set is **5 tasks (5.7 %)**. `qemu-alpine-ssh` is a task-image maintenance problem on the
benchmark side, not something the agent can fix; it should be excluded or rebuilt before it is used as
a capability signal again.

## Follow-up

- Two-point (2×/4×) axis A curve is now measured for the whole failure set. Next: fold it into the
  capability scoreboard once a standard run exists, and treat the 5 capability tasks as the P1 B-axis
  seeds (a long-horizon repo task template that resembles them).
- `qemu-alpine-ssh`: report the verifier dependency 404 to the benchmark task, and exclude it from the
  capability numbers until the image is rebuilt.
- P1 (axes B/C/D) stays blocked on the API-spend question; axis A cost is bounded and already spent.
