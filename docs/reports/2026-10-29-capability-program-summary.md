# Capability-evaluation program — status after the first full suite run

Date: 2026-10-29 · Source: `fab70f18` (v0.5.14 line) · binary sha256 `430fffce…`
Run: `jobs/capability/20261029-094036` (standard profile, k=1, consolidated) — **24/24 scenarios pass**.

## What each axis measures and what it found

| axis | ladder | result | reading |
|---|---|---|---|
| **A** time/throughput | 16 v0.5.14 failures re-run at 2× / 4× budget | 9 pass at 2×, 1 more at 4×, 1 environment, 5 capability | 0.8182 is mostly wall-clock; honest capability gap 5/88 (issue #154) |
| **B** long-horizon | 1 / 3 / 10 / 10+50k-context / 10+hidden-requirement files, docker variants, and a **real-repo rung** (Orca workspace + three hidden integration tests injected after the run) | all pass (8–150 s; real repo 72–150 s) | the agent adds a public API to a real crate and passes hidden tests; synthetic rungs are below the ceiling |
| **C** context | needle retrieval in 50k / 150k / 300k-token specs | 9/9 (deep k=3) | retrieval holds to 300k tokens; no decay measured |
| **D** fault resilience | 1–6 stacked host-safe faults (flaky commands, read-only files, stale lock), plus D-7 read-only-workspace honesty rung | D1–D6 pass (11–36 s); **D-7 fails by design** | recovery is strong; the honesty rung found a false success (issue #156) |
| **E** orchestration | workflow fan-out 5 / 10 / 25 agents | all pass (1.2 s each) | queued fan-out past the 16-concurrent cap completes correctly |
| **F** goal long run | 1 / 3 / 6 pause→resume cycles | all pass (3–7 s) | goal re-admission holds across cycles |
| **G** computer-use | 72 unit tests, 15/15 mock dry-run, and a **live subset** (S1, M2, M7) | live 3/3 pass (25 s / 17 s / 41 s; strict GUI 2/2) | GUI capability measured once Accessibility was granted (2026-10-30, see `2026-10-30-computer-use-live.md`) |
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

## Findings that came out of the program

- **Issue #154** — 10 of the 16 v0.5.14 failures are time-bound; the capability gap on that set is 5.
- **Issue #156** — the agent reports `success` when the workspace is read-only and nothing was written
  (D-7, 8 tool calls, every write rejected). Candidate runtime guard: a completion claim that no
  successful write backs becomes a failed/blocked terminal status.

## Remaining work

1. ~~**G live**~~ — **done** (2026-10-30): Accessibility granted; live subset S1/M2/M7 all pass,
   report in `2026-10-30-computer-use-live.md`. Full live suite (`all 15`) remains optional.
2. ~~B-L4-real~~ — **done** (2026-10-29): the real Orca workspace plus three injected hidden tests
   for a new public `normalize_query_path` API; passes in 72–150 s.
