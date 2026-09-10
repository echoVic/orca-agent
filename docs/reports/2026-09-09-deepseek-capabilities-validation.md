# DeepSeek Capability Validation

## Scope

Local implementation of cache-aware hierarchical context management, durable
terminal-output pages, file-defined agents, and shared ACP daemon sessions.
Product contracts are maintained in [the compaction report](2026-09-09-cache-aware-compaction.md),
[ADR 0006](../architecture/adr/0006-unified-exec-terminal-service.md),
[subagents](../subagents.md), and [ACP sessions](../acp-daemon.md).
No provider abstraction or worktree was created. Changes were split into logical
commits, fast-forwarded into local `main`, and the merged feature branch was
removed. No remote push or release was performed. The initial user commits
`028746cb` and `33c165c5` remain intact.

## Verification Environment

macOS, Rust 1.97.1, Node 22.23.1 and Python 3.11.8. Cargo commands use the existing target
directory, `CARGO_INCREMENTAL=0`, and at most two build jobs. Runs are serialized
at the build boundary. Temporary CLI sessions use isolated `ORCA_HOME` paths.
Missing credentials, remote-summary fallback, and interrupted tests are never
counted as passes. No coverage threshold was requested; coverage instrumentation
was skipped under the test workflow's `CHECK_COV_MODE=skip` rule.

## Verified Results

| Check | Result | Notes |
| --- | --- | --- |
| Core unit tests | 255 passed | Includes YAML parsing, trust, collision and multilingual definitions |
| Provider unit tests | 189 passed | After fixture socket/read and destructor cleanup correction |
| Runtime compaction | 30 passed | Includes durable snapshot and bounded/fallback behavior |
| Runtime subagent regression | 91 passed | Included again in the post-admission broad runtime run |
| Custom-agent focused tests | 6 passed | Separate-process worker, freeze, aliases and legacy hash compatibility |
| Production parent-restart contracts | 2 passed | Sync/detached origins, all mode transitions, frozen snapshots, source/latest task ownership and rejected foreign sessions |
| Output persistence unit tests | 17 passed | UTF-8 cursors, scope, limits, restart and unsafe paths |
| Output integration tests | 15 passed | Real 9 MiB capture plus reopen/read: 2.01 s, 4.47 MiB/s on this host |
| Terminal service | 13 passed | PTY/stdin, process cleanup, cursor and restart |
| ACP agent / RPC facade / subagent observability | 17 / 30 / 18 passed | Existing adapter contracts |
| Surface reducer contracts | 78 passed | Final runtime with recorded parent admission associations |
| ACP daemon process contracts | 7 passed | Includes accepted settings changes and shared-client behavior |
| Shared reverse-resource routing | 1 passed | Owner-only replies, spoofed observer replies, cancellation and disconnect |
| Attached TUI focused tests | 16 passed | Sequential permissions, acknowledgements, model changes, replay and draft recovery |
| TUI unit tests | 1,301 passed, 24 ignored | Includes final attached-client additions |
| Site production build | Passed | TypeScript, Vite and prerender |
| Release script tests | 8 passed | Fake/process harness tests, not publication |
| Terminal Bench adapter tests | 7 passed | Requires the repository's Python >= 3.11; system Python 3.9 cannot parse the type syntax |
| Version sync | Passed | Version remains 0.4.26; features are unpublished |
| Execution/platform boundary validators | Passed | Explicit inventory for new OS-specific sites |
| Surface validator and validator tests | Passed | Includes final TUI mutation inventory and export-order self-tests |
| Clippy all targets | Completed with warnings | Final diff-line check finds no warnings on added/modified lines; strict gate still fails on existing lint debt |

## Real Provider Evidence

`COMPACTION_REALAPI_RUNS=3 ORCA_SUMMARY_DEBUG=1 target/debug/examples/compaction_realapi`
completed three consecutive, fail-fast runs with no framework retries. The
fixture shrank from 7,472 estimated wire tokens to 1,325, 1,285 and 1,247.
Deterministic unchanged-prefix estimates were 53 tokens for eager micro reduction
and 7,233 tokens for cache-aware reduction. The latter retains more evidence and
also sends more total input. This is not a claim of universally lower billed
cost, nor a cold-cache causal experiment. API usage is reported separately.

`node scripts/release/real-api-acp-daemon.mjs` passed real DeepSeek streaming
through a shared daemon: standard observer output, competing-prompt rejection,
owner disconnection without cancelling ordinary model work, reconnect without
duplicate history, daemon restart recovery, and headless attachment.

`node scripts/release/real-api-custom-agent.mjs target/debug/orca` passed three
consecutive complete runs: discovery, deleted definition, `sync -> sync`,
`sync -> async`, `async -> async`, and `async -> sync`, each in a new parent
process. All 15 child executions returned their definition-only identifier.
The harness checks terminal checkpoint contents, immutable source task,
compatibility and frozen configuration, changing parent/latest task and attempt,
and increasing checkpoint sequence. Failed acceptance runs are not retried.
These CLI runs retain the normal transport retry policy; they are not claimed
as the zero-framework-retry compaction experiment above.

Earlier testing found two real implementation defects: synthetic loop task IDs
were incorrectly compared with registry root IDs, and detached admission was
incorrectly assumed to occur after lease acquisition. Explicit actor-committed
parent associations and mode-specific original revisions fix those defects.
One later API run with an ambiguous child task emitted a textual tool-call
attempt instead of its identifier; no tool ran and the harness failed. The
fixture task was clarified without weakening its output or permission checks.
The three consecutive passing runs use that unchanged, explicit fixture.

## Interaction Evidence

An actual TUI process under an isolated controlling PTY loaded a daemon-owned
transcript, accepted a prompt and rendered a response without client-side API
credentials. Computer Use was attempted: permissions were granted, but cmux and
Terminal returned menu-only accessibility trees and no screenshots. The cmux
CLI rejected access from a process outside cmux. No security preference was
changed. Screenshot-based acceptance is therefore not claimed.

## Residual Gate Failures

The exact release-profile workspace nextest run completed 3,139 tests: 3,070
passed, 68 failed, one timed out and seven were skipped. It also recorded one
flaky MCP transport cleanup test that passed on its second profile attempt.
One failure was a real test-isolation defect:
`exec_auto_model_defaults_to_pro` read the user's `~/.orca/config.toml`.
It now uses a private `ORCA_HOME` and passes an exact rerun. The remaining 68
failures all enter sandboxed command execution and fail because this host reports
`EnforcementUnavailable`; the HTTP allow test times out through all three
profile attempts. The release workflow runs these gates on Ubuntu 22.04 with
bubblewrap installed, so the macOS result is not substituted for that CI gate.

The earlier direct runtime run completed with 1,379 passes, seven failures and
seven existing ignored tests while excluding the HTTP allow test. Stateless
shutdown failed in an earlier run and passed in the final run. Workflow
cancellation failed in that broad run but passed five consecutive isolated
runs without test retries. The same basic-shell Seatbelt test fails in a
pre-change compiled binary on this host.

The strict `clippy -D warnings` gate stops on existing configuration, event and
tool-type lints. Non-strict all-target clippy completes. New warnings are fixed
within feature scope; unrelated lint cleanup is not bundled into these features.

Full-workspace `cargo fmt --all -- --check` passes. It required normalizing three
format-only blocks from the two pre-existing local commits; no runtime behavior
changed. The surface validator compares exports independent of source ordering.

Complete remaining runtime failure inventory:

```text
server::tests::command_exec_permission_profile_allowlist_miss_requests_permission_and_retries
server::tests::command_exec_permission_profile_denylist_block_reports_policy_denial
server::tests::command_exec_permission_profile_domain_policy_blocks_denied_http_request
server::tests::command_exec_permission_profile_domain_policy_blocks_localhost_resolution
server::tests::command_exec_permission_profile_domain_policy_blocks_unallowlisted_local_request
server::tests::command_exec_permission_profile_domain_policy_reports_blocked_host
workflow::host::tests::host_control_cancels_silent_workflow_promptly
```

Explicitly excluded after its earlier hang:
`server::tests::command_exec_permission_profile_domain_policy_allows_http_request`.
Earlier broad-run-only failure:
`server::tests::stateless_adapter_shutdown_cancels_joins_and_reaps_the_in_flight_turn`.

Complete release-profile integration failure inventory, under
`blade-deepseek::session_server_contract`:

```text
server_mode_bash_inherits_thread_active_permission_profile_network_policy
server_mode_bash_network_permission_allow_retries_with_grant
server_mode_command_exec_caps_buffered_output_by_bytes
server_mode_command_exec_caps_streaming_output_by_bytes
server_mode_command_exec_configured_permission_profile_enforces_network_domain_policy
server_mode_command_exec_configured_permission_profile_materializes_minimal_special_path
server_mode_command_exec_honors_cwd_and_env_overrides
server_mode_command_exec_list_returns_active_process_snapshots
server_mode_command_exec_preserves_legacy_script_command
server_mode_command_exec_read_caps_streaming_output
server_mode_command_exec_read_drains_streaming_output
server_mode_command_exec_rejects_duplicate_active_process_id
server_mode_command_exec_resize_rejects_zero_dimensions
server_mode_command_exec_respects_buffered_output_cap
server_mode_command_exec_returns_buffered_output
server_mode_command_exec_stops_active_processes_when_input_closes
server_mode_command_exec_streaming_respects_output_cap
server_mode_command_exec_streams_output_and_accepts_write
server_mode_command_exec_tty_supports_initial_size_and_resize
server_mode_command_exec_uses_session_network_domain_grants
server_mode_command_exec_with_process_id_can_be_terminated
server_mode_command_exec_write_requires_input_or_close
server_mode_controls_runtime_shell_session
server_mode_kills_runtime_shell_session
server_mode_lists_runtime_shell_sessions
server_mode_reads_runtime_shell_session_incrementally
server_mode_rejects_resize_for_pipe_shell_session
server_mode_request_permissions_session_scope_accepts_file_system_entries
server_mode_request_permissions_session_scope_accepts_workspace_roots_entries
server_mode_request_permissions_session_scope_persists_directory_grant
server_mode_request_permissions_waits_for_permission_response
server_mode_resizes_runtime_shell_pty_session
server_mode_session_network_deny_overrides_permission_profile_allow
server_mode_shell_read_honors_output_byte_cap
server_mode_shell_stops_active_process_group_when_input_closes
server_mode_starts_runtime_shell_pty_session_with_initial_size
server_mode_starts_runtime_shell_session_with_pty
server_mode_task_stop_reaps_runtime_shell_session
server_mode_turn_start_rebinds_runtime_workspace_roots_for_permission_grants
server_mode_updates_runtime_shell_session_description
```

Complete release-profile integration failure inventory, under
`blade-deepseek::shell_session_contract`:

```text
shell_session_applies_environment_overrides_and_unsets
sandboxed_shell_session_cannot_override_seatbelt_marker
shell_session_kill_preserves_already_exited_terminal_with_buffered_output
shell_session_kill_stops_running_task_and_collects_partial_output
shell_session_list_returns_running_shell_snapshots
shell_session_pty_exposes_terminal_to_child_process
shell_session_pty_starts_with_configured_window_size
shell_session_read_returns_incremental_output_without_waiting_for_exit
shell_session_reaps_task_stop_requests
shell_session_runs_interactive_stdin_and_records_task_result
shell_session_terminate_all_preserves_natural_completion
shell_session_updates_description_for_list_snapshots
```

Complete tool failure inventory, under `orca-tools::sandbox::seatbelt::tests`:

```text
path_cannot_override_seatbelt_executable
sandbox_allows_basic_shell_commands_and_null_device
sandbox_allows_writes_to_additional_roots
seatbelt_child_observes_nested_sandbox_environment
workspace_write_sandbox_allows_explicit_metadata_write_root
workspace_write_sandbox_allows_only_configured_unix_socket
workspace_write_sandbox_allows_workspace_git_reads_by_default
workspace_write_sandbox_allows_writes_under_slash_tmp
workspace_write_sandbox_can_terminate_a_child_process
```

## Unit-Test Workflow

Scope: durable output storage, terminal polling, shell capture and tool admission.
The scoped defect analysis found no pre-existing defect to relabel; persistent
replay is new behavior. Added cases cover cursor repeatability, UTF-8 boundaries,
EOF, truncation, restart, quotas and unsafe paths. Verification passed 17 storage
unit tests, 15 output integration tests and 13 terminal-service tests. Coverage
was intentionally skipped; `utree flush` was executed, but its collector did not
register the test files and reported a missing results directory. Counts above
come from Cargo, not that collector.
The additional parent-restart
contracts exercise production processes and are recorded separately above.

## Status

All three feature requirements have passing targeted and real-provider
acceptance. Final provider, ACP and parent-restart checks passed again after
lint cleanup. The known broad gate failures prevent treating this report as
release approval. Changes are committed on local `main` and unpublished at
version 0.4.26.
All registered temporary test roots and the coordinator state were removed
after the commands settled. The user's original Orca process, configuration,
sessions and commits were preserved.
