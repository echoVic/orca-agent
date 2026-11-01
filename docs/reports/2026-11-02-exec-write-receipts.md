# Exec write receipts: fix and live acceptance

Date: 2026-11-02 (Asia/Shanghai). Base: `ff2887d0`.

## Changes

- Restore the missing `writes_applied` argument in two core tests.
- Record successful `edit` / `write_file` calls in the existing persisted task registry. Aggregate new receipts through the current task tree, including detached workers and nested children.
- Retain the accounting baseline with the session's logical turn boundary across provider suspension and interrupted generations. New user turns start a new baseline. Hosted turns without a registered main task keep their transcript fallback.
- Older task records default to zero writes. The external field remains a boolean; status and exit-code behavior are unchanged.

## Regression evidence

The new subagent end-to-end test failed before the fix: the file contained `after`, but completion reported `false`. The hosted-turn regression separately caught a prior child write leaking into the next readonly turn. The resumed-generation regression caught a child write being lost after provider suspension, both with and without an intervening cancelled generation. All three were observed failing before their respective fixes and passing afterward.

The existing `canonical_hosted_edit_emits_committed_diff` also caught the synthetic-root error introduced during implementation; it passes after absent task identities are handled.

## Real DeepSeek acceptance

Provider: `deepseek`; model: `deepseek-flash`. Each scenario used an isolated workspace and ORCA_HOME, actual credentials, bounded execution, JSONL events, and independent file-content checks. Credentials were supplied through the environment and are absent from artifacts.

| Scenario | Expected / observed writes_applied | Exit | Result |
|---|---:|---:|---|
| Direct edit and read-back | true | 0 | pass |
| Default asynchronous child edit and parent read-back | true | 0 | pass |
| Explicit synchronous child edit and parent read-back | true | 0 | pass |
| Readonly run | false | 0 | pass |
| Blocker with one question | false | 5 | pass |
| Resume earlier successful write, then only read | false | 0 | pass |
| Answer blocked question, then edit and read-back | true | 0 | pass |

The normal subagent schema defaults to asynchronous execution. The synchronous compatibility run explicitly supplied `mode: "sync"`; the emitted raw tool arguments were checked, rather than inferring the mode from the prompt.

Binary SHA-256: `948245a1252343aff52f90e91b7e960371e5ce40ee7ab7344125a03159232ae0`.

Local evidence: `.superpowers/sdd/2026-11-02-exec-write-receipts/` in the primary checkout contains `live.py`, `followup.py`, `results.json`, `followup-results.json`, and each case's JSONL / stderr output. These ignored artifacts are not part of the commit.

## Gates

`cargo test --workspace -- --test-threads=1` completed with exit 0 on the final source. The runtime unit group passed 2,017 tests with 9 ignored; the CLI JSONL group passed all 32 tests, and the hosted-turn group passed all 75 tests. Remaining crate tests, integration tests and doc-tests also passed. Full output is saved as `workspace-tests.log` in the local evidence directory.

A parallel control run on the unchanged `ff2887d0` checkout, built in a separate target directory, produced 2,014 passing tests, 9 ignored tests and one failure: `server::tests::stateless_adapter_shutdown_cancels_joins_and_reaps_the_in_flight_turn` (`failed` instead of `cancelled`). The five baseline domain-policy tests passed when run separately with one test thread. Earlier parallel runs of the modified tree also showed intermittent server policy/process failures. Concurrent-suite stability is therefore not established by this acceptance run; the final workspace gate uses one test thread.

Independent code review found no remaining actionable issues after the generation-lifetime fixes. Formatting, diff whitespace checks and the runtime surface contract validator pass.

## Scope

Live evidence is from this macOS machine and these bounded real-model scenarios. It is not cross-platform or release validation. Shell-command writes remain outside the field's documented scope.
