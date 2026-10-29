# Capability-evaluation program — status after the first full suite run

Date: 2026-10-29 · Source: `fab70f18` (v0.5.14 line) · binary sha256 `430fffce…`
Run: `jobs/capability/20261029-094036` (standard profile, k=1, consolidated) — **24/24 scenarios pass**.

## What each axis measures and what it found

| axis | ladder | result | reading |
|---|---|---|---|
| **A** time/throughput | 16 v0.5.14 failures re-run at 2× / 4× budget | 9 pass at 2×, 1 more at 4×, 1 environment, 5 capability | 0.8182 is mostly wall-clock; honest capability gap 5/88 (issue #154) |
| **B** long-horizon | 1 / 3 / 10 / 10+50k-context / 10+hidden-requirement files, plus docker variants | all pass (8–49 s) | synthetic ladder below the ceiling; real-repo rung still to build |
| **C** context | needle retrieval in 50k / 150k / 300k-token specs | 9/9 (deep k=3) | retrieval holds to 300k tokens; no decay measured |
| **D** fault resilience | 1–6 stacked host-safe faults (flaky commands, read-only files, stale lock) | all pass (11–27 s) | recovery is strong; destructive faults (disk full, device lock) need the docker rung |
| **E** orchestration | workflow fan-out 5 / 10 / 25 agents | all pass (1.2 s each) | queued fan-out past the 16-concurrent cap completes correctly |
| **F** goal long run | 1 / 3 / 6 pause→resume cycles | all pass (3–7 s) | goal re-admission holds across cycles |
| **G** computer-use | mock dry-run (15 tasks) + 72 unit tests | protocol level only | **live blocked**: Accessibility not granted (`ax_trusted: false`); needs the user to allow it |
| **H** generalisation | B-L1 in Python, B-L2 renamed Rust | both pass (12–14 s) | no measurable memorisation drop at these rungs |

## Infrastructure that now exists

- `scripts/eval/capability/run.py` — on-demand runner: `--list`, `--axis`, `--rungs`, `--scenarios`,
  `--profile smoke|standard|deep`, `--failures last`, `--against last`, `--record`, `--issue`,
  `--max-usd`, and axis A (`--from-run`, `--multipliers`, `--execute`).
- Three execution backends: `local` (host), `docker` (Linux container with the musl binary),
  `harbor` (axis A). Scenario schema supports `prepare` hooks (large contexts, fault installation),
  `command: exec|workflow|goal`, `verify_env`, protected-file anti-cheat.
- Reports: markdown per run; `docs/reports/capability-scoreboard.json` records every recorded run.
- 18 unit tests for the library (Wilson, L50 with the separable-data guard, triage order, schema,
  report/diff).

## Remaining work

1. **G live** — blocked on the Accessibility grant on this Mac (persisting since the first round).
   Once granted: `python3 -I scripts/eval/computer_use/run.py --tasks S1,M2,M7 --out /tmp/orca-cu-live`.
2. **B-L4-real** — a real repository fixture inside the docker backend (the synthetic L4 passes, so
   the next meaningful rung is real code at ~50k tokens with a hidden behavioural requirement).
3. **D-destructive** — disk-full / read-only-device / port-conflict rungs using docker flags
   (`--tmpfs`, `:ro` mounts); the honesty variant (agent must report blocked instead of claiming
   success) is the most informative one.
