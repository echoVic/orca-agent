# Orca capability-evaluation program — design

Date: 2026-10-29
Status: draft for review

## Intent and why

Two things are wanted from this program, and they pull in slightly different directions:

1. **Find real problems and improve the agent.** The program must produce actionable findings — a
   minimal reproduction, an attribution (product bug / model limit / environment), and a re-runnable
   regression entry so a fix can be proven. A finding that cannot be re-run is not a finding.
2. **Know where the agent stands.** A stable scoreboard: per-axis difficulty ladders, the rung where
   success crosses 50 % (L50), cost per success, and the failure mix — comparable across versions the
   way the existing TB2 ledger is.

It is **run on demand** ("I'll run it when I want"), so nothing here needs a scheduler, a nightly
job, or a blocking CI gate. The design instead optimises for: cheap default runs, explicit profiles
for cost, and a comparison mode that answers "is this version better than the last one I measured".

Existing assets to build on: `scripts/eval/` (L1 sweep, mock provider, TB2 adapter, `summarize.py`),
`scripts/eval/computer_use/` (GUI harness), `docs/reports/evaluation-ledger.{md,json}` (the cross-round
record), Harbor 0.20.0 for container-backed tasks.

## Goals

- One command runs a chosen slice of the capability axes and writes a self-contained run directory.
- Every scenario has a deterministic verifier and a failure-diagnostic bundle.
- Every failure is attributed by rule, not by hand, into `capability_gap | time_bound |
  verification_strictness | product_bug | environment | flaky | harness`.
- One report per run: per-axis ladder table, L50 + CI, cost curves, failure taxonomy, diff vs the
  previous run of the same axis.
- One scoreboard file appended per run, and an optional ledger row (manual `--record`).
- Fix verification: re-run a named scenario (or a saved failure set) after a change and diff against
  the recorded baseline.

## Non-goals

- No scheduled/CI-gated runs. No dashboards. No public leaderboard submission.
- No replacement of TB2 or the L1 sweep; they stay as the external-validity and contract-regression
  layers. This program targets the **ceiling**, not the pass rate of known tasks.
- No human-graded scenarios: every rung is verified by code.
- No new GUI automation stack: computer-use reuses `scripts/eval/computer_use/`.

## Anti-goals (what would make this a failure even if it runs)

- A ladder where difficulty is not actually varied (e.g. all rungs fail for the same missing tool).
- Verifiers that the agent can read, edit, or satisfy trivially (answer files in the workspace).
- Numbers that move with the host (Docker noise, wall-clock spikes) being reported as capability.
- A "scoreboard" that mixes models or protocols without labelling it.
- Scenarios whose failure gives no diagnostic bundle — unfixable findings waste the loop.

## Measurement model

Four ceilings, measured by different axes:

| ceiling | question | axes |
|---|---|---|
| capability | what can it do at all, given unlimited budget? | B, C, E, F, G |
| time/throughput | is wall-clock the binding constraint? | A, B (deep profile) |
| reliability | can it repeat a success? does it self-verify? | D, H, all axes at k>=3 |
| containment | does it misbehave while succeeding? | D, G, H |

Shared metrics per scenario attempt: `outcome ∈ {pass, fail, error}`, `failure_class`, `turns`,
`tool_calls`, `tokens_in/out/cache`, `wall_seconds`, `verify_seconds`, `files_touched`,
`false_completion` (claimed done, verifier red), `needs_retry` (k>1 variance).

Ceiling estimates:

- **L50**: for each axis, fit a 2-parameter logistic `P(pass) = 1/(1+exp(-(a+b·d)))` over rung
  difficulty `d`, report `d50 = -a/b`, with a bootstrap CI over attempts. Axes with a single hard
  scenario instead report a pass-rate with Wilson interval.
- **Budget scaling (axis A)**: same task set at budget multipliers 1×/2×/4×; report Δpass and the
  per-task transition table. A task that passes at 4× but not at 1× is `time_bound`, not a
  capability gap.
- **Cost**: tokens and wall seconds per pass, by axis.
- **Reliability**: pass@k and pass^k (all attempts pass) at k=3.

Profiles (cost control; the runner refuses a profile that would exceed its scenario cap):

| profile | wall | k | purpose |
|---|---|---|---|
| smoke | <= 20 min | 1 | harness sanity after changing scenarios |
| standard | <= 3 h | 1 | normal on-demand round |
| deep | overnight | 3 | ceiling numbers and reliability, run rarely |

## Axes

Each axis is a directory of parameterised scenarios sharing one template. A scenario is the triple
`(fixture, prompt, verifier)` plus a `rung` parameter; the generator instantiates it per rung.

### A. Time/throughput scaling

- Hypothesis: part of the current 0.82 gap is wall-clock, not capability.
- Method: take the previous run's non-pass set (e.g. the 16 v0.5.14 failures), re-run each with the
  agent wall budget multiplied (task timeout override; where TB2 forbids it, a local synthetic
  budget-sensitive task family of equivalent shape). k=2 at 1×/2×/4×.
- Verifier: the task's own tests (unchanged — the point is to change only the budget).
- Output: per-task transition table and `time_bound` share.
- Acceptance: every failure either passes at higher budget (`time_bound`) or fails identically at 4×
  (`capability_gap`).

### B. Long-horizon engineering

- Hypothesis: context/plan decay over many files and turns is the main capability ceiling.
- Ladder (same repo pin, one template): L1 single-file function + test; L2 three files with a
  cross-file invariant (renamed public API); L3 ten-plus files + full test suite green + public
  contract untouched; L4 = L3 + evidence buried in 50 k tokens of context; L5 = L4 + a second
  requirement that is hidden (must be discovered from the codebase, not stated).
- Verifier: hidden tests injected after the run, plus a diff check for the public contract and an
  anti-cheat scan (tests not modified).
- Diagnostics: failing test names, files touched vs expected, compile/test cycle count.
- Acceptance: monotone L1..L3; L50 identified between two rungs.

### C. Context and compaction

- Hypothesis: facts decay across compactions; the agent "runs" but forgets.
- Method: seed a long session (200–1000 turns) with three planted facts early (a constraint, a file
  path, a naming rule); later turns require all three. Measure fact-recall rate per compaction
  boundary vs turn index, not just task success.
- Verifier: the final deliverable plus explicit recall probes (verifier checks the artefact obeys
  the planted facts).
- Acceptance: recall curve vs turn index; a drop identifies the compaction budget that breaks.

### D. Fault resilience

- Hypothesis: single injected faults are recovered; combinations degrade sharply.
- Method: fault-injection ladder of 1..6 stacked faults (command timeout, locked file, port already
  in use, intermittent 5xx, flaky test, permission denied, disk full). Reuse the mock provider's
  existing fault modes plus environment faults.
- Verifier: task-specific; additionally assert no residue (processes, ports, temp files) and no
  out-of-scope file changes.
- Output: recovery rate vs fault count, recovery cost (extra turns/tokens), containment failures.
- Acceptance: identifies the fault count where recovery collapses.

### E. Tool and subagent orchestration

- Hypothesis: workflow fan-out correctness degrades before capability does.
- Ladder: fan-out 5/10/25/50 agents over a shared tree with conflict points, plus phases; separate
  rungs kill one phase mid-run and require `restart-failed` / `restart-phase` to reuse only the
  cached results that remain valid.
- Verifier: file-state assertions after completion/restart, cache-hit checks (no duplicate agent
  work), conflict resolution correctness.
- Acceptance: identifies fan-out/complexity where correctness breaks; restart semantics pass/fail.

### F. Goal mode long run

- Hypothesis: goal continuation drifts or stalls on long objectives; no-progress and budget guards
  fire at the wrong time.
- Ladder: objectives needing 50/150/400 outer turns; injected drift (model does unrelated work at
  turn 10); repeated pause/resume; budget caps.
- Verifier: end state of the artefact plus goal DB assertions (terminal state, turns, budget use).
- Output: completion rate, stall rate (paused without progress), false terminal claims.
- Acceptance: completion vs turn-scale curve; no stall below the budget cap.

### G. Computer-use (live GUI)

- Hypothesis: GUI success collapses with steps/recovery requirements before single-app tasks do.
- Ladder: existing S1–S8/M1–M7 as rung 0; add cross-app composites (read a value in app A, act in B),
  recovery tasks (wrong window focused, modal dialog), and undo tasks; measure success, steps,
  screenshots, mis-actions, recovery success.
- Precondition: Accessibility + screen recording granted (currently `ax_trusted: false`); until then
  only the mock smoke runs.
- Verifier: harness-native checks (`computer_use/verify.py`) extended per rung.

### H. Generalization / anti-memorisation

- Hypothesis: some pass rate is memorised; re-skinning drops it.
- Method: take B/D/E scenarios and re-skin: different language, different repo layout, renamed
  domain terms, different ordering. Compare pass rate on the skin vs original.
- Acceptance: report the delta; > 20 pp drop is a finding about memorisation.

## Shared infrastructure

```
scripts/eval/capability/
  run.py                 # CLI entry point
  scenarios/
    A-budget/scenario.json
    B-long-horizon/L1..L5/
    C-context/
    D-fault/
    E-orchestration/
    F-goal/
    G-computer-use/
    H-skin/
  lib/
    scenario.py          # schema load/validate, fixture instantiation
    backends.py          # `local` (host) and `harbor` (container) execution
    verify.py            # shared verifier helpers (run tests, diff, anti-cheat scan)
    triage.py            # failure classification rules + report rendering
    metrics.py           # L50 fit, Wilson intervals, cost curves
    report.py            # run report + scoreboard append + baseline diff
    schema.json          # JSON schema for scenario + result
  tests/                 # unit tests for the library (stdlib unittest)
```

Scenario schema (v1):

```json
{
  "schema_version": 1,
  "id": "B-long-horizon-L3",
  "axis": "B",
  "rung": 3,
  "difficulty": {"files": 12, "turns_hint": 40},
  "backend": "harbor",
  "fixture": "repos/pinned-orca-v0.5.14/",
  "prompt": "prompts/L3.md",
  "verify": ["pytest -q", "python3 verify/contract.py"],
  "limits": {"wall_seconds": 1800, "tool_calls": 200, "tokens_out": 200000},
  "anti_cheat": ["tests_unmodified", "no_answer_files", "public_contract_unchanged"],
  "diagnostics": ["workspace_diff", "test_log", "trajectory"]
}
```

Runner CLI:

```
run.py --list
run.py --axis A --profile standard --k 2            # one axis
run.py --axis B,D --rungs L1-L3 --profile smoke      # slice
run.py --scenarios B-long-horizon-L3,D-fault-6       # exact set
run.py --failures last                               # re-run the last run's failures
run.py --compare <run-id> --against last             # diff report
run.py --record                                      # append scoreboard + ledger row
```

Result directory: `jobs/capability/<run-id>/{run.json, per-scenario/<id>/{result.json, artifacts/}}`,
`jobs/` stays gitignored. `run.py --record` writes `docs/reports/<date>-capability-<run-id>.md` and
one line into `evaluation-ledger.json` under a new `capability` key — kept separate from the TB2
mean so the two never mix.

## Triage and attribution

Every non-pass attempt is classified by rule, in this order:

1. `harness` — the scenario itself errored (setup/verify crash); blocks the run summary, must be fixed
   before numbers count.
2. `environment` — verifier could not start, image pull failed, host noise (detected by preflight
   fingerprint: docker version, load, disk, and a canary scenario).
3. `time_bound` — the budget-scaled rerun at 4× passes (axis A evidence), or the run hit the wall
   limit with a green/partial verifier.
4. `product_bug` — the runtime/agent contract misbehaved independent of task difficulty: crash,
   policy violation, session-end kills a declared workspace service, lost trajectory, false
   completion with no work, repeated identical tool calls. These are the findings that become issues.
5. `verification_strictness` — the agent's artefact is arguably right but the verifier demands a
   specific form; recorded as a finding about either the agent's precision or the verifier.
6. `capability_gap` — everything else: it tried, it did not solve it.

Only `product_bug` becomes an issue automatically, with the diagnostic bundle attached and a
`run.py --scenarios <id>` command in the issue body. `capability_gap`/`time_bound` feed the
scoreboard, not issues.

## Anti-cheat rules

- Verifiers live outside the workspace the agent can write (injected after the run, or in a read-only
  mount); the agent never sees test files.
- A scan fails the scenario if the agent modified the fixture's tests, wrote an answer file named by
  the verifier, or touched files outside the declared scope.
- Prompts never name the hidden requirement (axis B L5).
- The run's protocol block (commit, binary SHA-256, model, effort, profile, host fingerprint) is
  mandatory in every report; runs without it are not recorded.
- Repeated runs use different seeds where the scenario supports it (H axis uses paired skins).

## Regression protocol after a fix

1. Reproduce: `run.py --scenarios <id> --profile smoke` shows fail.
2. Fix lands (product code, not the scenario).
3. Reinstate: same command passes; `run.py --compare <new> --against <baseline>` shows the transition.
4. Keep the scenario; it becomes part of that axis's regression set (cheap rungs in `smoke`).

## Phasing

| phase | deliverable | rough effort |
|---|---|---|
| P0 | `lib/` + `run.py` + scenario/result schema + report + axis A (budget scaling on the v0.5.14 failure set) | 1–2 days |
| P1 | axes B (L1–L3), C, D with diagnostics and triage; smoke/standard profiles honest | 3–5 days |
| P2 | axes E, F (orchestration, goal long run) | 3–4 days |
| P3 | axis G live (needs AX grant) + axis H skins | 2–3 days |
| P4 | scoreboard/trend report, `--failures last`, docs; first full deep run | 2 days |

Each phase ends with a committed report and a ledger entry; the runner and library are committed with
the phase, not before.

## Calls I have made (review these)

- Scenarios are data (`scenario.json`) + small verifier scripts, not code generators — easier to
  review and to keep anti-cheat visible.
- `local` backend for context/fault/goal/workflow/mock-GUI; `harbor` backend only for B (real repo
  tasks) and A where TB2 timeouts matter — keeps standard runs under ~3 h.
- Computer-use live is its own phase and is gated on the Accessibility grant.
- Scoreboard lives under `docs/reports/` (JSON next to the ledger) rather than a new top-level
  directory.
- Capability numbers never merge into the TB2 mean; the ledger lists them as a separate block.

## Builder-check resolutions (2026-10-29)

A fresh builder reviewed this document and raised eight questions. Resolutions:

**Answered by the human partner in chat**

- Q8/issues: findings are filed as GitHub issues ("发现的问题还是提 issue"). The runner does not
  run a background filer; it prints a "product findings" section with a ready-to-paste issue body
  (`run.py --issue <scenario-id>`), and the human or the agent files it. Scenario ids are stable so
  the same bug deduplicates.

**My calls (check these while reading)**

- Q1 (axis B stack): the fixture is Orca's own repository pinned at a commit — Rust/cargo, not a
  `pytest` repo (the schema example was sloppy). Verification runs `cargo nextest run --profile ci`
  scoped to the touched crates, plus a hidden contract test injected after the run. Bounded to
  ~10 min per attempt by scoping; "full suite green" in L3 means the scoped suite plus build.
- Q2 (local vs container): `local` stays the host for C, E, F, H and mock-GUI — they need no
  destructive fault and their verifiers already live outside the workspace. A third backend
  `docker` is added for B and D, where the faults (disk full, permission denied, locked files) and
  the cargo toolchain need isolation; images stay under ~2 GB.
- Q4 (axis E rung): the rung parameter is **total agents across phases**, not concurrent agents, so
  the rungs do not collide with the 16-concurrent cap; conflict points and restart assertions are
  phase-based.
- Q5 (evidence standard): standard runs are k=1 and report Wilson intervals per rung only. L50 is
  deep-only (k>=3); the report prints "L50 needs k>=3" when it cannot be computed. A profile's wall
  cap is **per invocation** (one axis slice), and `--record` writes one scoreboard entry per
  invocation; `--compare --against last` compares the most recent invocation with overlapping
  scenario ids.
- Q6 (shared assets): the capability program does not edit the shared L1 probe, `mock_provider.py`,
  `computer_use/`, or the TB2 adapter/quarantine. Fault composition lives in
  `capability/lib/faults.py` wrapping the provider; axis G extends `computer_use/verify.py` with a
  separate capability verifier rather than changing the existing one.
- Q1/Q2 sub-question on images: axis B's fixture repo is ~500 MB checked out; the cargo toolchain
  image is pinned by digest; nothing exceeds 2 GB.

**Still needs the human partner**

1. API spend: C (200–1000 turns), F (400 outer turns) and B L3–L5 at k=3 need a real provider key and
   real dollars; the profiles now cap wall time per invocation, but not spend. What is the ceiling
   for one deep run (e.g. "$20 per full deep run, stop when exceeded")? Until answered, C/F/B-deep do
   not run; the mock-backed axes and axis A (cheap) do.
2. Axis G on this machine: live GUI automation needs the Accessibility grant, and cross-app composites
   need dedicated apps. Should G run live on this Mac (with which apps), or wait for a dedicated
   machine/VM? Until answered, P3 keeps only the mock smoke run.
