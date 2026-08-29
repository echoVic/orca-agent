#!/usr/bin/env node

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  parseManifestText,
  validateCurrentInventory,
  validateManifest,
  validatePortableTestFixtures,
} from "./validate-windows-platform-boundaries.mjs";

const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const normalizeLineEndings = (source) => source.replace(/\r\n?/g, "\n");
const readNormalizedSource = (relativePath) =>
  normalizeLineEndings(
    readFileSync(path.join(repoRoot, relativePath), "utf8"),
  );
assert.equal(
  normalizeLineEndings("pull_request:\r\n  push:\r\n"),
  "pull_request:\n  push:\n",
  "workflow contracts must be independent of checkout line endings",
);
const manifestPath = path.join(
  repoRoot,
  "docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json",
);
const baseline = JSON.parse(readFileSync(manifestPath, "utf8"));

assert.throws(
  () => parseManifestText('{"schema_version":'),
  /malformed manifest JSON/,
  "malformed manifests must fail with a stable diagnostic",
);

{
  const candidate = structuredClone(baseline);
  candidate.deferred_boundaries.push(candidate.deferred_boundaries[0]);
  assert.throws(
    () => validateManifest(candidate),
    /duplicate boundary id/,
    "duplicate reviewed boundary ids must fail",
  );
}

{
  const candidate = structuredClone(baseline);
  candidate.operation_patterns = candidate.operation_patterns.filter(
    ([id]) => id !== "non_unix_lock_stub",
  );
  assert.throws(
    () => validateManifest(candidate),
    /reviewed operation pattern set drift/,
    "the manifest cannot weaken scanning by deleting an operation class",
  );
}

{
  const candidate = structuredClone(baseline);
  candidate.operation_patterns[0][1] = "never-match-this-pattern";
  assert.throws(
    () => validateManifest(candidate),
    /reviewed regex drift/,
    "the manifest cannot weaken scanning by changing a reviewed regex",
  );
}

{
  const candidate = structuredClone(baseline);
  candidate.deferred_boundaries[0][4] = "unowned-future-plan";
  assert.throws(
    () => validateManifest(candidate),
    /unknown deferred owner/,
    "every deferral must point at a closed implementation phase",
  );
}

{
  const candidate = structuredClone(baseline);
  candidate.deferred_boundaries[0][1] = "crates/missing.rs";
  assert.throws(
    () => validateCurrentInventory(candidate, { repoRoot }),
    /inventory path does not exist/,
    "missing reviewed paths must fail",
  );
}

{
  const candidate = structuredClone(baseline);
  candidate.deferred_boundaries[0][3] += 1;
  assert.throws(
    () => validateCurrentInventory(candidate, { repoRoot }),
    /reviewed platform operation count drift/,
    "stale reviewed counts must fail",
  );
}

{
  const relativePath = "crates/orca-runtime/src/goal_actor.rs";
  const original = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const sourceOverrides = new Map([
    [
      relativePath,
      `${original}\nfn regression() { let _ = std::process::Command::new("sh"); }\n`,
    ],
  ]);
  assert.throws(
    () => validateCurrentInventory(baseline, { repoRoot, sourceOverrides }),
    /unreviewed direct platform operation/,
    "source overrides must pass through the same inventory scanner",
  );
}

validateManifest(baseline);
validateCurrentInventory(baseline, { repoRoot });

{
  const relativePath = "crates/orca-runtime/src/visible_fixture_module.rs";
  const sourceOverrides = new Map([
    [
      relativePath,
      String.raw`
#[cfg(test)]
pub(crate) mod tests {
    fn non_portable_fixture() {
        let _ = PathBuf::from("/tmp/visible");
    }
}
`,
    ],
  ]);
  assert.throws(
    () => validatePortableTestFixtures({ repoRoot, sourceOverrides }),
    /host-canonical-path/,
    "portable fixture validation must scan visible cfg(test) modules",
  );
}

{
  const relativePath = "crates/orca-runtime/src/combined_cfg_fixture.rs";
  const sourceOverrides = new Map([
    [
      relativePath,
      String.raw`
#[cfg(all(test, unix))]
mod unix_tests {
    fn non_portable_fixture() {
        let _ = ["sh", "-lc"];
    }
}
`,
    ],
  ]);
  assert.throws(
    () => validatePortableTestFixtures({ repoRoot, sourceOverrides }),
    /direct-unix-command-argv/,
    "portable fixture validation must scan cfg expressions that include test",
  );
}

{
  const relativePath = "crates/orca-runtime/src/raw_protocol_fixture.rs";
  const sourceOverrides = new Map([
    [
      relativePath,
      String.raw`
#[cfg(test)]
mod tests {
    fn non_portable_raw_protocol_fixture() {
        let _ = r#"{"command":["sh","-lc","true"]}"#;
    }
}
`,
    ],
  ]);
  assert.throws(
    () => validatePortableTestFixtures({ repoRoot, sourceOverrides }),
    /direct-unix-command-argv/,
    "portable fixture validation must reject Unix argv embedded in raw protocol requests",
  );
}

{
  const relativePath = "crates/orca-runtime/src/multiple_fixture_modules.rs";
  const sourceOverrides = new Map([
    [
      relativePath,
      String.raw`
#[cfg(test)]
mod first_tests {
    fn non_portable_fixture() {
        let _ = CanonicalPath::try_new(PathBuf::from("/tmp/first")).unwrap();
        // windows-platform-boundary: protocol-shape-only
        let _ = r#"wire shape with braces { "command": ["sh", "-lc"] }"#;
        /* nested comment { /* inner } */ still ignored } */
    }
}

#[cfg(test)]
mod tests {
    fn portable_fixture() {
        let _ = test_canonical_path("second");
    }
}
`,
    ],
  ]);
  assert.throws(
    () => validatePortableTestFixtures({ repoRoot, sourceOverrides }),
    /host-canonical-path/,
    "portable fixture validation must scan every cfg(test) module regardless of its name",
  );
}

{
  const relativePath = "crates/orca-runtime/src/portable_fixture_tests.rs";
  const sourceOverrides = new Map([
    [
      relativePath,
      String.raw`
#[cfg(test)]
mod tests {
    fn non_portable_fixtures() {
        let _ = PathBuf::from("/tmp/orca-test");
        let _ = ["sh", "-lc"];
        let _ = std::path::PathBuf::from( "/tmp/second" );
        let _ = PathBuf::from("/tmp-orca-prefix");
        let _ = vec![String::from("sh"), String::from("-lc")];
        let _ = ["sh".into(), "-lc".to_owned()];
        let _ = ["bash", "-lc"];
    }
}
`,
    ],
  ]);
  assert.throws(
    () => validatePortableTestFixtures({ repoRoot, sourceOverrides }),
    (error) => {
      const fixtureDiagnostics = error.message
        .split("\n")
        .filter((line) => line.includes(relativePath));
      return (
        fixtureDiagnostics.filter((line) => line.includes("host-canonical-path"))
          .length === 3 &&
        fixtureDiagnostics.filter((line) =>
          line.includes("direct-unix-command-argv"),
        ).length === 4
      );
    },
    "portable fixture validation must reject every reviewed host-specific test pattern",
  );
}

{
  const relativePath = "crates/orca-runtime/src/portable_fixture_tests.rs";
  const sourceOverrides = new Map([
    [
      relativePath,
      String.raw`
#[cfg(test)]
mod tests {
    fn portable_fixtures() {
        let _ = test_canonical_path("orca-test");
        let _ = platform_command_argv();
        // Documentation example only: ["sh", "-lc"]
        /* Another documentation example: vec!["bash", "-lc"] */
        // windows-platform-boundary: protocol-shape-only
        let _ = vec!["sh".to_string(), "-lc".to_string()];
    }
}
`,
    ],
  ]);
  assert.doesNotThrow(
    () => validatePortableTestFixtures({ repoRoot, sourceOverrides }),
    "portable fixture validation must accept centralized host-platform helpers",
  );
}

const migratedLockBoundaries = [
  ["crates/orca-runtime/src/goal_actor.rs", "goal runtime"],
  ["crates/orca-runtime/src/runtime_surface/store.rs", "runtime surface owner"],
  ["crates/orca-runtime/src/thread_store/writer.rs", "thread store writer"],
];
for (const [relativePath, label] of migratedLockBoundaries) {
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  assert.ok(
    source.includes("use orca_platform::fs") &&
      source.includes("ExclusiveFileLock::"),
    `${label} must use the cross-platform exclusive file lock`,
  );
}
const closedLockBoundaryIds = new Set([
  "goal-actor-unix-lock",
  "goal-actor-non-unix-lock-stub",
  "runtime-surface-owner-flock-ffi",
  "runtime-surface-non-unix-lock-stub",
  "thread-writer-flock-ffi",
  "thread-writer-non-unix-lock-stub",
]);
for (const [boundaryId] of baseline.deferred_boundaries) {
  assert.ok(
    !closedLockBoundaryIds.has(boundaryId),
    `${boundaryId} must be removed after migrating to the cross-platform lock`,
  );
}
const runtimeSurfaceSource = readFileSync(
  path.join(repoRoot, "crates/orca-runtime/src/runtime_surface/store.rs"),
  "utf8",
);
assert.ok(
  /atomic_write\(\s*path,/.test(runtimeSurfaceSource),
  "runtime surface owner epochs must use the cross-platform atomic replacement primitive",
);
assert.ok(
  !baseline.foundation_exceptions.some(
    ([boundaryId]) => boundaryId === "runtime-surface-temp-rename",
  ),
  "runtime surface temp rename must leave the foundation exception list",
);

const atomicPersistenceBoundaries = [
  [
    "crates/orca-core/src/config/folder_trust.rs",
    "atomic_write",
    "folder trust",
  ],
  [
    "crates/orca-runtime/src/workflow/state.rs",
    "atomic_write",
    "workflow state",
  ],
  ["crates/orca-runtime/src/tasks.rs", "atomic_write", "task state"],
  [
    "crates/orca-runtime/src/workflow/command.rs",
    "atomic_write",
    "workflow launch record",
  ],
  [
    "crates/orca-runtime/src/thread_store/writer.rs",
    "atomic_write_with",
    "thread transcript rewrite",
  ],
  [
    "crates/orca-runtime/src/workflow/ipc.rs",
    "atomic_write",
    "workflow IPC state",
  ],
];
for (const [relativePath, primitive, label] of atomicPersistenceBoundaries) {
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  assert.ok(
    source.includes(primitive),
    `${label} must use the cross-platform atomic replacement primitive`,
  );
}
const closedTempRenameBoundaryIds = new Set([
  "folder-trust-temp-rename",
  "workflow-state-temp-rename",
  "tasks-temp-rename",
  "workflow-command-temp-rename",
  "thread-writer-temp-rename",
  "workflow-ipc-temp-rename",
]);
for (const [boundaryId] of baseline.foundation_exceptions) {
  assert.ok(
    !closedTempRenameBoundaryIds.has(boundaryId),
    `${boundaryId} must leave the foundation exception list`,
  );
}

const atomicJobSpawnContracts = [
  ["crates/orca-core/src/verification.rs", "launch_user_trusted("],
  ["crates/orca-mcp/src/transport.rs", "launch_user_trusted("],
  ["crates/orca-runtime/src/hooks.rs", "launch_user_trusted("],
  ["crates/orca-runtime/src/subagent_async_worker.rs", "launch_user_trusted("],
  ["crates/orca-runtime/src/workflow/host.rs", "launch_user_trusted("],
  ["crates/orca-runtime/src/shell_session.rs", "broker.launch(process, capability)"],
  ["crates/orca-tools/src/bash.rs", "spawn_with_capability("],
  ["crates/orca-tools/src/external.rs", "spawn_with_capability("],
  ["crates/orca-tools/src/git.rs", "spawn_user_trusted("],
  ["crates/orca-tools/src/grep.rs", "spawn_user_trusted("],
];
for (const [relativePath, marker] of atomicJobSpawnContracts) {
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  assert.ok(
    source.includes(marker),
    `${relativePath} must enter the execution broker before creating its Windows child`,
  );
}
const verificationSource = readFileSync(
  path.join(repoRoot, "crates/orca-core/src/verification.rs"),
  "utf8",
);
assert.match(
  verificationSource,
  /fn kill_child_tree_without_job\(child: &mut Child\) \{[\s\S]*?#\[cfg\(not\(windows\)\)\][\s\S]*?child\.kill\(\)/,
  "verifier cleanup must not bypass Windows Job Object ownership",
);

const processSource = readFileSync(
  path.join(repoRoot, "crates/orca-platform/src/process.rs"),
  "utf8",
);
const shellResolverSource = readFileSync(
  path.join(repoRoot, "crates/orca-platform/src/shell/resolve.rs"),
  "utf8",
);
assert.ok(
  shellResolverSource.includes("is_current_directory_executable"),
  "Windows shell lookup must reject executables from the current directory",
);
assert.ok(
  shellResolverSource.includes("resolve_program"),
  "Windows process lookup must resolve PATHEXT launcher shims",
);
assert.ok(
  readFileSync(path.join(repoRoot, "crates/orca-mcp/src/transport.rs"), "utf8").includes("resolve_program(command)"),
  "MCP stdio launches must use the Windows PATHEXT-aware program resolver",
);
const toolProcessSource = readFileSync(
  path.join(repoRoot, "crates/orca-tools/src/process.rs"),
  "utf8",
);
for (const marker of [
  "command.creation_flags(CREATE_SUSPENDED)",
  "AssignProcessToJobObject(job.handle, process)",
  "resume_process_threads(child.id())",
  "spawn_named_or_inherited",
]) {
  assert.ok(
    processSource.includes(marker),
    `Windows atomic Job spawn must contain ${marker}`,
  );
}
assert.ok(
  processSource.indexOf("AssignProcessToJobObject(job.handle, process)") <
    processSource.indexOf("resume_process_threads(child.id())"),
  "Windows child must enter its Job Object before any thread resumes",
);
assert.ok(
  !toolProcessSource.includes("let _process_job = ProcessJob::attach(child_pid)"),
  "process waiters must receive the Job lease created at spawn time",
);

const terminalSource = readFileSync(
  path.join(repoRoot, "crates/orca-platform/src/terminal.rs"),
  "utf8",
);
assert.ok(
  !terminalSource.includes("portable_pty"),
  "Windows ConPTY ownership must not use a spawn-before-Job abstraction",
);
for (const marker of [
  "PROC_THREAD_ATTRIBUTE_JOB_LIST",
  "ProcessJob::create_unassigned(job_name)",
  "attributes.set_job(process_job.raw_handle())",
  "STARTF_USESTDHANDLES",
]) {
  assert.ok(
    terminalSource.includes(marker),
    `Windows ConPTY spawn must contain ${marker}`,
  );
}
for (const marker of ["CREATE_SUSPENDED", "ResumeThread(thread.raw())", "ProcessJob::attach(info.dwProcessId)"]) {
  assert.ok(
    !terminalSource.includes(marker),
    `Windows ConPTY spawn must atomically assign its Job Object instead of using ${marker}`,
  );
}

const clipboardSource = readFileSync(
  path.join(repoRoot, "crates/orca-tui/src/clipboard.rs"),
  "utf8",
);
for (const marker of ["OpenClipboard", "CF_UNICODETEXT", "SetClipboardData"]) {
  assert.ok(
    clipboardSource.includes(marker),
    `Windows clipboard fallback must use the Unicode clipboard API: ${marker}`,
  );
}
assert.ok(
  !clipboardSource.includes('"powershell.exe"'),
  "Windows clipboard fallback must not spawn PowerShell",
);

const sandboxSource = readFileSync(
  path.join(repoRoot, "crates/orca-runtime/src/shell_session.rs"),
  "utf8",
);
for (const marker of ["verify_setup", "SETUP_HELPER_VERSION"]) {
  assert.ok(
    sandboxSource.includes(marker),
    `Windows sandbox runtime must validate setup receipts: ${marker}`,
  );
}
const setupHelperSource = readFileSync(
  path.join(repoRoot, "crates/orca-windows-sandbox-setup/src/main.rs"),
  "utf8",
);
assert.ok(
  setupHelperSource.includes("ensure_appcontainer_profile"),
  "Windows setup helper must provision the stable AppContainer profile",
);
const runnerSource = readFileSync(
  path.join(repoRoot, "crates/orca-windows-runner/src/main.rs"),
  "utf8",
);
assert.ok(
  runnerSource.includes("native_runner_launches_absolute_windows_program"),
  "Windows runner must include a real native execution contract",
);
for (const marker of ["forward_stdin", "MAX_FORWARDED_STDIN_BYTES", "spawn_named_or_inherited"]) {
  assert.ok(
    runnerSource.includes(marker),
    `Windows runner must enforce its bounded stdin/job contract: ${marker}`,
  );
}
const asyncWorkerSource = readFileSync(
  path.join(repoRoot, "crates/orca-runtime/src/subagent_async_worker.rs"),
  "utf8",
);
for (const marker of [
  "spawn_async_subagent_worker_via_runner",
  "orca-windows-runner.exe",
  "forward_stdin",
  "contains_process(pid)",
]) {
  assert.ok(
    asyncWorkerSource.includes(marker),
    `Windows async workers must use the runtime-owned runner boundary: ${marker}`,
  );
}
assert.doesNotMatch(
  asyncWorkerSource,
  /WindowsRunnerLaunchRequest[\s\S]{0,180}serde\(rename_all = "camelCase"\)/,
  "Windows runner request fields must retain the snake_case protocol names",
);
assert.ok(
  setupHelperSource.includes("native_setup_provisions_and_checks_profile_receipt"),
  "Windows setup helper must include a real native profile contract",
);
for (const marker of ["SetupOperation::Repair", "SetupOperation::Remove", "repair_setup", "remove_setup"]) {
  assert.ok(
    setupHelperSource.includes(marker),
    `Windows setup helper must expose lifecycle operation ${marker}`,
  );
}
const capabilitySource = readFileSync(
  path.join(repoRoot, "crates/orca-windows-sandbox/src/capabilities.rs"),
  "utf8",
);
for (const marker of ["Sha256", "receipt_path", "verify_setup_for_workspace"]) {
  assert.ok(
    capabilitySource.includes(marker),
    `Windows setup receipts must be workspace-scoped: ${marker}`,
  );
}
const runtimeBashSource = readFileSync(
  path.join(repoRoot, "crates/orca-runtime/src/runtime_bash.rs"),
  "utf8",
);
assert.ok(
  runtimeBashSource.includes("domain-restricted network sandbox is unavailable"),
  "Windows domain-restricted network policy must fail closed until direct bypass is enforced",
);
const serverSource = readFileSync(
  path.join(repoRoot, "crates/orca-runtime/src/server.rs"),
  "utf8",
);
assert.ok(
  serverSource.includes("Windows domain-restricted network sandbox is unavailable"),
  "Windows command/exec domain network policy must fail closed until direct bypass is enforced",
);

const workflowPath = path.join(repoRoot, ".github/workflows/windows-ci.yml");
assert.ok(existsSync(workflowPath), "native Windows CI workflow must exist");
const workflow = normalizeLineEndings(readFileSync(workflowPath, "utf8"));
const releaseWorkflowPath = path.join(repoRoot, ".github/workflows/release.yml");
assert.ok(existsSync(releaseWorkflowPath), "release workflow must exist");
const releaseWorkflow = normalizeLineEndings(readFileSync(releaseWorkflowPath, "utf8"));
const installerSource = readFileSync(path.join(repoRoot, "install.ps1"), "utf8");
const pullRequest = workflow.match(/  pull_request:\n([\s\S]*?)\n  push:/);
assert.ok(pullRequest, "Windows CI must validate pull requests before merge");
const push = workflow.match(/  push:\n([\s\S]*?)\n\npermissions:/);
assert.ok(push, "Windows CI must validate relevant main-branch pushes");
for (const marker of [
  "branches: [main]",
  '"npm/orca/**"',
  '"install.ps1"',
  '"scripts/release/**"',
  '".github/workflows/release.yml"',
  '".github/workflows/windows-ci.yml"',
]) {
  assert.ok(
    pullRequest[1].includes(marker),
    `Windows pull-request trigger must contain ${marker}`,
  );
}
for (const marker of [
  '"npm/orca/**"',
  '"install.ps1"',
  '"scripts/release/**"',
  '".github/workflows/release.yml"',
]) {
  assert.ok(
    push[1].includes(marker),
    `Windows push trigger must contain ${marker}`,
  );
}
for (const marker of [
  "windows-latest",
  "windows-11-arm",
  "aarch64-pc-windows-msvc",
  "shell: pwsh",
  '$ErrorActionPreference = "Stop"',
  "$PSNativeCommandUseErrorActionPreference = $true",
  "node scripts/test-validate-windows-platform-boundaries.mjs",
  "cargo check --workspace --all-targets --locked",
  "cargo clippy --workspace --all-targets --locked",
  "taiki-e/install-action@nextest",
  "cargo nextest run -p orca-tui --lib --locked --profile ci-serial",
  "cargo nextest run --test tui_pty_contract --locked --profile ci-serial --no-tests=pass",
  "cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast",
  "cargo build --release --locked",
  "target/release/orca.exe",
  "--version",
  "restricted_windows_pty_session_keeps_terminal_and_resizes",
  "orca-windows-runner",
  "orca-windows-sandbox-setup",
  "prompt_names_powershell_7_as_the_active_shell_dialect",
  "altgr_char_is_normalized_to_text_input_on_windows_only",
  "windows_standalone_update_uses_downloaded_powershell_installer",
  "windows_npm_update_waits_for_running_orca_before_replacing_package",
]) {
  assert.ok(workflow.includes(marker), `Windows CI workflow must contain ${marker}`);
}
const windowsX64Job = workflow.match(/  native-x64:\n([\s\S]*?)\n  native-arm64:/);
assert.ok(windowsX64Job, "Windows CI must define a native x64 job");
const windowsArm64Job = workflow.match(/  native-arm64:\n([\s\S]*)$/);
assert.ok(windowsArm64Job, "Windows CI must define a native ARM64 job");
for (const [job, label] of [
  [windowsX64Job[1], "x64"],
  [windowsArm64Job[1], "ARM64"],
]) {
  const runnerBuild = "cargo build -p orca-windows-runner --locked";
  const fullSuite = "cargo nextest run --workspace --all-targets --locked";
  assert.ok(
    job.includes(runnerBuild),
    `Windows ${label} CI must materialize the async-worker runner before integration tests`,
  );
  assert.ok(
    job.indexOf(runnerBuild) < job.indexOf(fullSuite),
    `Windows ${label} CI must build the runner before the full test suite`,
  );
}
const nextestConfig = readFileSync(
  path.join(repoRoot, ".config/nextest.toml"),
  "utf8",
);
for (const marker of [
  "[profile.ci]",
  'default-filter = "not (binary(=orca_tui) | binary(=tui_pty_contract))"',
  "fail-fast = false",
  "test-threads = 2",
  "retries = 2",
  'slow-timeout = { period = "60s", terminate-after = 2 }',
  "[profile.ci-serial]",
  "test-threads = 1",
  "threads-required = 2",
  "capability_mark_rails",
  "older_incomplete_background_completion",
  "external_tool_timeout_",
  "in_flight_summary_request_stops_waiting_for_headers_when_cancelled",
  "workflow_pause_resume_and_clone_control_persisted_run",
  "server_mode_input_eof_cancels_pending_permission_request",
  "production_connection_enforces_terminal_wait_canonical_result_limit",
  "cancelling_prompt_terminalizes_outstanding_read_text_file_call",
  "concurrent_terminals_from_one_tool_keep_cleanup_identity_exact",
  "hook_timeout_kills_descendant_processes",
  "subagent_batch_cancellation_stops_blocked_hook_and_unstarted_sibling",
  "bash_commands_receive_eof_on_stdin_instead_of_inheriting_terminal",
  "command_exec_streaming_filesystem_sandbox_denial_requests_permission_and_retries",
  "binary(=task_output_store)",
  "verifier_command_timeout_kills_descendant_processes",
  "server_mode_interrupt_cancels_active_bash_tool_wait_and_accepts_next_turn",
  "server_mode_reads_runtime_shell_session_incrementally",
  "surface_goal_max_inner_continuation_is_durable_before_successor_execution",
  "workflow_submit_streams_background_result",
  "operation_panic_has_one_terminal_and_actor_reclaims_thread_state",
  "panicking_goal_run_settles_outer_turn_and_fails_closed_to_paused",
  "runtime_host_launches_saved_workflow_without_blocking_the_next_turn",
  "binary(=subagent_contract)",
  "binary(=session_server_contract)",
  "test(/^acp::supervisor::tests::/)",
  "failed_private_winner_append_retries_before_capability_reroute",
  "distinct_capability_loss_is_reconciled_after_retained_retry",
  "typed_task_foreground_commits_ownership_before_registry_visibility",
]) {
  assert.ok(
    nextestConfig.includes(marker),
    `nextest CI profile must contain ${marker}`,
  );
}
const acpSupervisorSource = readNormalizedSource(
  "crates/orca-runtime/src/acp/supervisor.rs",
);
assert.ok(
  acpSupervisorSource.includes(
    "#[cfg(windows)]\n    const TEST_TIMEOUT: Duration = Duration::from_secs(10);",
  ),
  "Windows ACP protocol tests must allow the ARM64 runner ten seconds per frame",
);
const runtimeHostContractSource = readNormalizedSource(
  "crates/orca-runtime/tests/runtime_host.rs",
);
assert.ok(
  runtimeHostContractSource.includes(
    "#[cfg(windows)]\nconst TEST_TIMEOUT: Duration = Duration::from_secs(10);",
  ),
  "Windows runtime host integration tests must allow ten seconds for process-backed workflows",
);
const taskRegistrySource = readNormalizedSource(
  "crates/orca-runtime/src/tasks.rs",
);
for (const marker of [
  "ExclusiveFileLock::acquire(&self.index_lock_path())",
  "ExclusiveFileLock::acquire(&target.index_lock_path())",
  "ProcessJob::open_named(&async_worker_job_name(agent_id))",
  ".contains_process(pid)",
  "job.terminate(137)",
]) {
  assert.ok(
    taskRegistrySource.includes(marker),
    `task persistence and recovered workers must contain ${marker}`,
  );
}
const providerSource = readNormalizedSource(
  "crates/orca-provider/src/lib.rs",
);
const runtimeHostSource = readNormalizedSource(
  "crates/orca-runtime/src/runtime_host.rs",
);
assert.ok(
  runtimeHostSource.includes(
    "Err(mpsc::TryRecvError::Disconnected) => {\n                                Some((HostShutdownActorState::NeedsDispatch, None))",
  ),
  "host shutdown must recheck an actor that exits after accepting idempotent shutdown",
);
assert.ok(
  providerSource.includes(
    "const STREAM_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50)",
  ),
  "synchronous provider cancellation must not wait on a one-second receive poll",
);
assert.ok(
  pullRequest[1].includes('".config/nextest.toml"') &&
    push[1].includes('".config/nextest.toml"'),
  "Windows CI must run when the nextest profile changes",
);

for (const relativePath of [
  "tests/session_server_contract.rs",
  "tests/subagent_contract.rs",
]) {
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  assert.ok(
    source.includes("Start-Sleep -Milliseconds"),
    `${relativePath} must use the active Windows shell dialect for sleep hooks`,
  );
}
const subagentContract = readFileSync(
  path.join(repoRoot, "tests/subagent_contract.rs"),
  "utf8",
);
const asyncSubagentLaunchContract = subagentContract.match(
  /fn async_subagent_launches_without_blocking_parent_tool\(\) \{([\s\S]*?)\n}/,
);
assert.ok(
  asyncSubagentLaunchContract,
  "async subagent launch contract test must remain present",
);
assert.ok(
  asyncSubagentLaunchContract[1].includes("async subagent launch failed") &&
    asyncSubagentLaunchContract[1].includes(
      "String::from_utf8_lossy(&output.stdout)",
    ) &&
    asyncSubagentLaunchContract[1].includes(
      "String::from_utf8_lossy(&output.stderr)",
    ),
  "async subagent launch failures must report captured stdout and stderr",
);
const sessionServerContract = readFileSync(
  path.join(repoRoot, "tests/session_server_contract.rs"),
  "utf8",
);
const incrementalShellReadContract = sessionServerContract.slice(
  sessionServerContract.indexOf(
    "fn server_mode_reads_runtime_shell_session_incrementally()",
  ),
  sessionServerContract.indexOf(
    "fn server_mode_shell_read_honors_output_byte_cap()",
  ),
);
assert.ok(
  incrementalShellReadContract.includes(
    'assert_eq!(output_delta["final"], false)',
  ) &&
    incrementalShellReadContract.includes(
      'assert_eq!(update["status"], "running")',
    ) &&
    !incrementalShellReadContract.includes(
      "read_sent_at.elapsed() < Duration::from_millis(500)",
    ),
  "incremental shell reads must be proven by protocol state instead of runner wall-clock speed",
);
const workflowCliContract = readFileSync(
  path.join(repoRoot, "tests/workflow_cli_contract.rs"),
  "utf8",
);
assert.ok(
  workflowCliContract.includes("mock_stream_delay_ms"),
  "tests/workflow_cli_contract.rs must use the provider-native cross-platform delay",
);
assert.ok(
  workflowCliContract.includes("wait_until_active"),
  "tests/workflow_cli_contract.rs must wait for persisted active state instead of racing worker startup",
);
assert.ok(
  workflowCliContract.includes(
    'wait_for_workflow_status(temp.path(), Some(&home), task_id, "running")',
  ),
  "workflow pause/resume must wait for the persisted worker to be running before requesting pause",
);
const pauseResumeFixture = workflowCliContract.slice(
  workflowCliContract.indexOf(
    "fn workflow_pause_resume_and_clone_control_persisted_run()",
  ),
  workflowCliContract.indexOf(
    "fn workflow_restart_commands_launch_from_persisted_run_record()",
  ),
);
assert.ok(
  pauseResumeFixture.includes("agent('first', { minHoldMs: 6000 })") &&
    !pauseResumeFixture.includes("agent('mock_stream_delay_ms 6000')"),
  "workflow pause/resume must create its pause window at the workflow runner boundary",
);
const runtimeLifecycleContract = readNormalizedSource(
  "tests/runtime_lifecycle_contract.rs",
);
assert.ok(
  runtimeLifecycleContract.includes("Duration::from_secs(10)") &&
    runtimeLifecycleContract.includes("printf before; sleep 30; printf after"),
  "Windows shell cancellation must stay bounded well below the fixture's natural completion",
);
const bashToolSource = readNormalizedSource("crates/orca-tools/src/bash.rs");
const noisyCancelFixture = bashToolSource.slice(
  bashToolSource.indexOf(
    "fn noisy_streaming_cancel_does_not_deadlock_reader_shutdown()",
  ),
  bashToolSource.indexOf(
    "fn bash_command_allows_additional_working_directory_writes()",
  ),
);
assert.ok(
  noisyCancelFixture.includes("execute_host_test_streaming_with_policy_or_cancel("),
  "Windows noisy streaming cancellation must exercise the shared process core through the host test command",
);
const runtimeHostContract = readNormalizedSource(
  "crates/orca-runtime/tests/runtime_host.rs",
);
const generationApprovalFixture = runtimeHostContract.slice(
  runtimeHostContract.indexOf(
    "fn generation_scoped_approval_handler_controls_canonical_tool_execution()",
  ),
  runtimeHostContract.indexOf(
    "fn request_scoped_approval_handler_remains_the_hosted_fallback()",
  ),
);
assert.ok(
  generationApprovalFixture.includes(
    'HostedTurnRequest::new("edit notes.txt :: old => approved")',
  ) &&
    generationApprovalFixture.includes(
      'event["payload"]["status"] == "completed"',
    ),
  "generation-scoped approval must use a portable successful write fixture and keep its completed terminal assertion",
);
const nativeLockBehaviorTests = [
  "goal_runtime_lease_is_shared_in_process_and_exclusive_across_processes",
  "thread_and_policy_owner_leases_fail_closed_and_wall_rollback_has_no_authority",
  "history_rename_search_and_compress_work_for_latest",
];
for (const marker of nativeLockBehaviorTests) {
  assert.ok(
    windowsX64Job[1].includes(marker),
    `Windows x64 CI must run the native lock behavior ${marker}`,
  );
  assert.ok(
    windowsArm64Job[1].includes(marker),
    `Windows ARM64 CI must run the native lock behavior ${marker}`,
  );
}
const releaseWindowsX64Gate = releaseWorkflow.match(
  /- name: Run native Windows x64 behavior gates[\s\S]*?(?=\n      - name:)/,
);
assert.ok(releaseWindowsX64Gate, "release workflow must define a native Windows x64 gate");
const releaseWindowsArm64Gate = releaseWorkflow.match(
  /- name: Run native Windows ARM64 behavior gates[\s\S]*?(?=\n      - name:)/,
);
assert.ok(
  releaseWindowsArm64Gate,
  "release workflow must define a native Windows ARM64 gate",
);
for (const gate of [releaseWindowsX64Gate[0], releaseWindowsArm64Gate[0]]) {
  assert.ok(gate.includes("shell: pwsh"), "release Windows behavior gates must use PowerShell 7");
  assert.ok(
    gate.includes('$ErrorActionPreference = "Stop"') &&
      gate.includes("$PSNativeCommandUseErrorActionPreference = $true"),
    "release Windows behavior gates must fail on every native command error",
  );
}
for (const marker of [
  "prompt_names_powershell_7_as_the_active_shell_dialect",
  "altgr_char_is_normalized_to_text_input_on_windows_only",
  "windows_standalone_update_uses_downloaded_powershell_installer",
  "windows_npm_update_waits_for_running_orca_before_replacing_package",
  ...nativeLockBehaviorTests,
]) {
  assert.ok(
    releaseWindowsX64Gate[0].includes(marker),
    `release x64 gate must run the native Windows behavior contract ${marker}`,
  );
  assert.ok(
    releaseWindowsArm64Gate[0].includes(marker),
    `release ARM64 gate must run the native Windows behavior contract ${marker}`,
  );
}
for (const marker of [
  "orca-windows-runner.exe",
  "orca-windows-sandbox-setup.exe",
  'Copy-Item "LICENSE"',
  'Compress-Archive -Path "$stage/*"',
]) {
  assert.ok(releaseWorkflow.includes(marker), `release workflow must package ${marker}`);
}
for (const marker of [
  "orca-windows-runner.exe",
  "orca-windows-sandbox-setup.exe",
  '"LICENSE"',
  "SetupSandbox",
  "RepairSandbox",
  "RemoveSandbox",
  "echoVic/orca-agent",
]) {
  assert.ok(installerSource.includes(marker), `Windows installer must handle ${marker}`);
}
assert.ok(
  installerSource.indexOf("if ($RemoveSandbox)") <
    installerSource.indexOf("$target = Get-OrcaTarget"),
  "Windows sandbox removal must not require a release download",
);
for (const marker of ['operation = "remove"', '"repair"', '"provision"']) {
  assert.ok(
    installerSource.includes(marker),
    `Windows installer must dispatch setup lifecycle operation ${marker}`,
  );
}
console.log("windows platform boundary validator tests passed");
