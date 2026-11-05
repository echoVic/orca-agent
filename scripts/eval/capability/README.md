# Capability evaluation evidence

The existing local/Docker runner and independent verifiers remain the evaluation boundary. No new dependencies are required. Run artifacts use schema version 2; old runs remain readable, with unavailable evidence reported as `unknown`.

## Requested execution and observed metadata

Optional `execution` scenario fields and CLI overrides use the real supported settings: `model`, `reasoning_effort` (`low`, `high`, `max`), `endpoint`, `max_turns`, `max_tool_calls`, `max_cost_usd`, `max_wall_time_secs`. CLI values override scenario values. Model/budgets become exec flags; effort and endpoint become runtime environment settings. Unspecified runtime defaults are `unknown`. The local runner removes inherited provider configuration and uses a private ORCA_HOME. Mock runs always use their local endpoint.

Protocol records requested model/effort, safe endpoint origin, full binary SHA256 and source commit. Attempt records hash the actual prepared fixture, source fixture, prompt, prepare script, available verifier inputs/tests and verifier command. Provider request/response captures contribute observed model/effort, sampling values and tool-schema SHA256 only when present. Response model and provider/engine fingerprints remain `unknown` when the provider does not expose them; router labels are not response evidence. Safe endpoint identity excludes userinfo, path, query and fragment. Distinct paths on one origin are deliberately indistinguishable.

`provider-requests.jsonl` may contain existing raw capture data; the protocol extractor allowlists metadata and never copies authorization. The current real provider does not supply request/response captures to this harness, so unexposed metadata remains `unknown`. A tool schema hash is available only from an actual captured `tools` list.

## Discovery and final verification

B-L3 and B-L4 document fixtures now declare their rounding policy evidence. A first discovery requires a paired successful `read_file`, `grep` or `file_search` request/result, an exact source path boundary, and the constraint marker in tool output. Failed/unpaired calls, writes, path decoys and final response text cannot establish discovery. Other tools, including shell commands, are conservatively not classified as discovery. Each first record includes source, call id/name, event line, output SHA256, tools needed and last observed cumulative input/output tokens. Token values are an observation before discovery, not exact per-constraint attribution; absent usage is `unknown`.

Discovery is independent of `final_verification`: passing final state can have no discovery evidence, and a valid discovery can coexist with a failed final verifier and false completion. Verifier file inputs are snapshotted before the local agent executes. Original tests, scenario/prepare/verifier files and snapshots are guarded against byte changes. This is an evidence guard, not OS isolation against a hostile host process. Existing non-file verifier commands retain their original execution semantics.

## Controlled offline fixtures

- `C-multi-constraint-full` supplies a long document; `C-multi-constraint-compacted` supplies a controlled compacted document. Both independently verify seven answer fields: path, number, prohibited actions, unresolved approval, supporting evidence, opposing evidence and decision, plus preserved archive bytes. Their prepare scripts preserve identical constraints. This compares supplied context variants; it does not demonstrate that the runtime itself performed compaction.
- `G-1-external-state` prepares a SQLite ticket database outside the workspace. The app script updates it; a separate read-only verifier checks the exact independently persisted row. A completion claim in `done.txt` fails. This is reproducible offline application state, with no GUI claim.
- The generic mock endpoint is a reply fixture, not a solver for these tasks. Offline tests exercise real Python runner/backend functions and subprocess fixture scripts, replacing provider startup with an offline endpoint stub. Repeat-grouping tests also start the real local HTTP fixture on two distinct ports. Observed fixture usage is synthetic test input, not paid model results.

## Repeated runs and fixed evaluation sets

`evaluation.split` is `holdout`, `tuning` or `unknown`; `suite_id` names a fixed suite. CLI split/suite values can fill unspecified metadata but cannot relabel a declared set. Manifest hashes bind scenario identities and membership. Aggregation never pools different splits, suites, manifests, binary/source versions, requested or observed request configurations, schema/verifier/fixture hashes, response labels or fingerprints. Unknown manifests are isolated per run. Fixed manifests identify a set; organizational controls must keep tuning decisions away from holdout outcomes.

Groups report pass rate, unbiased binary outcome variance, Wilson 95% confidence interval, false completions and discoveries separately. Cost per successful task sums **all attempts, including failures**, divided by passes. Missing any attempt cost/usage makes the corresponding complete total per pass `unknown`; observed cost subtotal and observation counts remain available. No passes gives `unknown`, not zero cost. Emitted runtime cost is an observed estimate, not an invoice. The legacy fallback token-price estimate is used only as a spending stop guard and never fills missing measured cost.

## Compression and provider event observations

`parse_events` retains `context.compaction.started` markers and records each actual `context.compacted` completion with reason, strategy, message counts and `payload.observation`. Completed measurements preserve before/after token estimates, elapsed time, summary requests/usage/completeness/cost, returned-service cache ratios and local prefix reuse estimates. Provider observation records accept both `provider.response.completed` and the current runtime name `model.response.completed`; each stores event type/line and `elapsed_ms`, `first_delta_ms`, `first_delta_kind`. Additive observation fields remain available. Missing/null fields become `unknown`; invalid top-level numeric measurements become `unknown`, while real zero and false values remain known.

Local and Docker execution preserve `compactions` and `provider_observations` in `Attempt.as_dict()`, top-level `attempt.json`, parsed event artifacts and `run.json` attempts, including failed verifier outcomes. Repeated-run groups summarize completed events across all attempts, with observed/missing counts, means/totals and known-subset `observed_mean`/`observed_total`. Started markers do not count as completed compactions. Missing any recorded event field makes the complete mean/total `unknown`. Empty records or old runs provide no measurement, rather than proof of zero calls; `attempts_with_records` states how many attempts expose the list. A cache-ratio mean is an arithmetic mean of per-event ratios, not a token-weighted aggregate service cache-hit ratio.

The report shows both group statistics and per-attempt observation records. Before/after tokens and prefix reuse are local estimates, not service KV-cache proof. Provider elapsed time covers the logical provider call with retry waits; first delta is the first nonempty streamed reasoning/text/tool event (current `first_delta_kind` values `reasoning`, `text`, `tool`), not necessarily a visible answer token. These timings do not measure per-token generation speed or isolate the runtime's overhead. Summary cost remains separate from cumulative `usage.updated`/`session.completed` attempt cost to avoid double counting. Summary expenses outside that cumulative stream are therefore not included in the existing cost/pass numerator; the report explicitly states this boundary.

## Offline verification and limits

```sh
python3 -B -m unittest discover -s scripts/eval/capability/tests -v
python3 -B scripts/eval/capability/run.py --dry-run --scenarios C-multi-constraint-full,C-multi-constraint-compacted,G-1-external-state --binary /private/tmp/orca-eval-missing-binary --out /private/tmp/orca-eval-dry-run
python3 -B scripts/eval/capability/run.py --dry-run --scenarios S0-smoke-pass,S0-smoke-fail --binary /private/tmp/orca-eval-missing-binary --out /private/tmp/orca-eval-smoke-plan
python3 -B scripts/eval/capability/run.py --aggregate /path/to/run-root --out /private/tmp/orca-eval-aggregate
```

RED evidence is `/private/tmp/orca-eval-red.log`: initial 36 tests failed (11 failures, 4 errors); subsequent boundary and offline Docker-argv regression failures were retained. Observation integration first ran 56 tests with 7 expected failures before implementation. GREEN evidence is `/private/tmp/orca-eval-green.log`. The final focused suite has 60 tests (original 56 plus four repeat-grouping regressions), including real prepare/verifier subprocesses, repeated fake-binary backend executions, aggregation CLI and provider/event record fixtures. Observation regressions cover both event names, missing/null/invalid/zero values, pass/fail artifact retention, completed-only aggregation, failed-attempt measurements, partial means and report output. Docker observation retention is exercised only via offline subprocess capture.

No paid API, real model capability, Docker runtime, native GUI, actual runtime compaction or Cargo/workspace validation is claimed. Goal/TUI execution remains the existing specialized harness and rejects explicit overrides it cannot apply; workflow rejects unsupported exec budget overrides. Source-only Docker parameter capture verifies configuration/auth construction, not container behavior. Broader Python/workspace checks belong to the integrating main thread.

Controlled local mock repeats preserve each actual `endpoint_identity` in attempt provenance and the group's `endpoint_identities`. For comparison only, `endpoint_comparison_identity` is `controlled-mock:<provider_fixture_sha256>`, fingerprinting the actual mock provider script bytes and `MOCK_WATCH` behavior configuration. Different fixture fingerprints stay separate. This port normalization requires provider `mock`, backend `local`, a known matching fingerprint and the runner's loopback origin; real providers and legacy/unknown mock records keep their actual origin in the comparison key. Old runs without captured fixture fingerprints are not retroactively relabelled.

The aggregation key also checks `scenario_sha256`, `prompt_sha256`, `prepare_sha256`, `verifier_command_sha256`, and `source_fixture_sha256` alongside prepared fixture/verifier bytes, even when the supplied manifest hash is unchanged. Evaluation identity supplies these fields only when attempt/run protocol does not contain them.

Port-grouping RED reran the evidence module with real loopback fixtures: six assertion failures plus one missing-field error. GREEN then ran all 60 tests, followed by the built debug Orca CLI against the mock provider for `S0-smoke-pass,S0-smoke-fail --k 2`: two groups with two attempts each, two passes and two expected verifier failures. Actual ports remain recorded; the failed group retains observed cost and reports per-pass cost as `unknown` because it has no passes. Existing mock subprocess teardown can emit a ResourceWarning after kill; the test does not claim process cleanup correctness.

Only files listed in `CHANGED_FILES.txt` are owned by this evaluation change. All edits used shell/Python; no commit or push was performed.
