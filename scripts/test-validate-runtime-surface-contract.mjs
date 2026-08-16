#!/usr/bin/env node

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import * as validator from "./validate-runtime-surface-contract.mjs";

const {
  assertNoProductionRuntimeSurfaceSiblingGlobs,
  parseManifestText,
  parseRuntimeSurfacePublicExports,
  parseSurfaceFacadeExports,
  unstableSurfaceReferenceLines,
  validateArtifactBundle,
  validateArtifactDigest,
  validateCurrentInventories,
  validateManifestStructure,
  validateRuntimeSurfaceContract,
} = validator;

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = path.join(
  repoRoot,
  "docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json",
);
const digestPath = path.join(
  repoRoot,
  "docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json",
);
const baseline = JSON.parse(readFileSync(manifestPath, "utf8"));
const productionMutationSites = [
  ...validator.scanTuiMutationEntrypoints({ repoRoot }),
];

function cloneManifest() {
  return structuredClone(baseline);
}

function expectFailure(label, run, pattern) {
  assert.throws(run, pattern, label);
}

function validateCandidate(manifest) {
  return validateManifestStructure(manifest, { reviewedManifest: baseline });
}

function expectReviewedDrift(label, mutate) {
  const manifest = cloneManifest();
  mutate(manifest);
  expectFailure(label, () => validateCandidate(manifest), /reviewed manifest .* drift/);
}

expectFailure(
  "wildcard runtime-surface exports are rejected",
  () => parseRuntimeSurfacePublicExports("pub use commands::*;"),
  /public exports must be explicit.*commands::\*/,
);

assert.deepEqual(
  parseRuntimeSurfacePublicExports(
    "pub use commands::{AdmissionOutput, SurfaceCommand};\npub use store::{SurfaceStore};",
  ),
  {
    commands: ["AdmissionOutput", "SurfaceCommand"],
    store: ["SurfaceStore"],
  },
);
assert.deepEqual(parseRuntimeSurfacePublicExports("pub use host::RuntimeHost;"), {
  host: ["RuntimeHost"],
});
assert.deepEqual(
  parseSurfaceFacadeExports(
    "pub mod surface { pub use crate::runtime_surface::{SurfaceCursor, SurfaceEvent}; }",
  ),
  ["SurfaceCursor", "SurfaceEvent"],
);
expectFailure(
  "wildcard surface facade exports are rejected",
  () => parseSurfaceFacadeExports("pub use crate::runtime_surface::*;"),
  /facade exports must be explicit/,
);

expectFailure(
  "production sibling globs are rejected",
  () => assertNoProductionRuntimeSurfaceSiblingGlobs("use super::*;", "commands"),
  /production imports must be explicit.*use super::\*/,
);
assert.doesNotThrow(() =>
  assertNoProductionRuntimeSurfaceSiblingGlobs(
    "#[cfg(test)]\nmod tests { use super::*; }",
    "commands",
  ),
);

assert.deepEqual(
  unstableSurfaceReferenceLines(
    "// unstable_surface in a comment\nconst NAME: &str = \"unstable_surface\";\nuse crate::unstable_surface::SurfaceCursor;",
  ),
  [3],
);

function appSourceOverride(extraSource) {
  const relativePath = "crates/orca-tui/src/app.rs";
  const absolutePath = path.join(repoRoot, relativePath);
  return new Map([[relativePath, `${readFileSync(absolutePath, "utf8")}\n${extraSource}\n`]]);
}

function sourceOverride(relativePath, source) {
  return new Map([[relativePath, source]]);
}

function expectUnlistedRuntimeMutation(label, functionName, body) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(`fn ${functionName}() { ${body} }`),
      }),
    new RegExp(`unlisted mutation-capable TUI entrypoint ${functionName}`),
  );
}

for (const relativePath of [
  "crates/orca-runtime/tests/runtime_surface_manifest.rs",
  "crates/orca-tui/src/surface_boundary_tests.rs",
]) {
  assert.doesNotMatch(
    readFileSync(path.join(repoRoot, relativePath), "utf8"),
    /std::process::Command|Command::new\s*\(|repository_(?:manifest_)?validator|validate-runtime-surface-contract\.mjs/,
    `${relativePath} must remain process-independent and leave repository validation to Node tests`,
  );
}

expectFailure(
  "malformed JSON is rejected",
  () => parseManifestText('{"schema_version":'),
  /malformed manifest JSON/,
);

{
  const manifest = cloneManifest();
  manifest.artifact_bundle.private_contract_sha256 = "0".repeat(64);
  expectFailure(
    "artifact hash mismatches are rejected",
    () => validateArtifactBundle(manifest, { repoRoot }),
    /private contract SHA-256 mismatch/,
  );
}

{
  const digest = JSON.parse(readFileSync(digestPath, "utf8"));
  digest.artifacts[0].sha256 = "0".repeat(64);
  expectFailure(
    "reviewed artifact digest mismatches are rejected without commit metadata",
    () => validateArtifactDigest(digest, { repoRoot }),
    /artifact digest SHA-256 mismatch for .*private-contract\.md/,
  );
}

{
  const manifest = cloneManifest();
  manifest.source_facts.push(structuredClone(manifest.source_facts[0]));
  expectFailure(
    "duplicate inventory ids are rejected",
    () => validateCandidate(manifest),
    /source_facts contains duplicate id/,
  );
}

{
  const manifest = cloneManifest();
  manifest.source_facts[0].pop();
  expectFailure(
    "rows must match declared column widths",
    () => validateCandidate(manifest),
    /source_facts row 0 has width/,
  );
}

{
  const manifest = cloneManifest();
  const cancel = manifest.thread_commands.find((row) => row[0] === "CancelOperation");
  cancel[2] = "UnknownCommandTarget";
  expectFailure(
    "command targets must be closed",
    () => validateCandidate(manifest),
    /CancelOperation has unknown target/,
  );
}

{
  const manifest = cloneManifest();
  const resume = manifest.thread_commands.find((row) => row[0] === "ResumeOperation");
  resume[9] = [];
  expectFailure(
    "mutation commands must retain required acknowledgements",
    () => validateCandidate(manifest),
    /ResumeOperation has no required acknowledgements/,
  );
}

{
  const manifest = cloneManifest();
  manifest.task_status_transitions[0][1] = "UnknownStatus";
  expectFailure(
    "transition endpoints must belong to a closed state inventory",
    () => validateCandidate(manifest),
    /task_status_transitions has unknown target state UnknownStatus/,
  );
}

{
  const manifest = cloneManifest();
  manifest.phase_0a_manifest_invariants[0] = "placeholder invariant text";
  expectFailure(
    "the exact reviewed invariant registry is closed",
    () => validateCandidate(manifest),
    /unknown phase_0a_manifest_invariant: placeholder invariant text/,
  );
}

{
  const manifest = cloneManifest();
  manifest.operation_terminal_mapping[0][0] = "Completed(Failed)";
  expectFailure(
    "impossible completed-failed terminal sources are rejected",
    () => validateCandidate(manifest),
    /operation terminal mapping contains impossible source Completed\(Failed\)/,
  );
}

{
  const manifest = cloneManifest();
  manifest.closed_inventory.surface_commit_batch_limits.canonical_encoded_byte_limit = 16_777_216;
  expectFailure(
    "the reviewed surface batch byte limit cannot be weakened",
    () => validateCandidate(manifest),
    /surface batch canonical byte limit must be 8388608/,
  );
}

{
  const manifest = cloneManifest();
  manifest.operation_transitions[0][2] = "";
  expectFailure(
    "operation transition triggers are required",
    () => validateCandidate(manifest),
    /operation_transitions row 0 has no trigger/,
  );
}

{
  const manifest = cloneManifest();
  const permissionRoute = manifest.jsonl_routing_matrix.find(
    (row) => row[0] === "permission/respond",
  );
  permissionRoute[1] = "DirectThreadResponder";
  expectFailure(
    "JSONL permission ownership remains on the opaque router",
    () => validateCandidate(manifest),
    /permission\/respond must be owned by OpaquePermissionRouter/,
  );
}

{
  const manifest = cloneManifest();
  manifest.acp_projection_matrix[0][1] = "UnknownAcpDisposition";
  expectFailure(
    "ACP projection dispositions are closed",
    () => validateCandidate(manifest),
    /unknown ACP projection disposition UnknownAcpDisposition/,
  );
}

{
  const manifest = cloneManifest();
  manifest.thread_commands[0][3][0] = "UnknownCapability";
  expectFailure(
    "command capabilities are closed",
    () => validateCandidate(manifest),
    /ReserveOperation has unknown capability UnknownCapability/,
  );
}

{
  const manifest = cloneManifest();
  manifest.thread_commands[0][12][0] = "UnknownCommandError";
  expectFailure(
    "command errors are closed",
    () => validateCandidate(manifest),
    /ReserveOperation has unknown command error UnknownCommandError/,
  );
}

expectReviewedDrift("ACP projection row identities are exact", (manifest) => {
  manifest.acp_projection_matrix[0][0] = "Invented.Fact";
});

expectReviewedDrift("operation edges and triggers are exact", (manifest) => {
  manifest.operation_transitions[0][1] = "Suspended";
  manifest.operation_transitions[0][2] = "Invented.OperationTrigger";
});

expectReviewedDrift("operation terminal dispositions are exact", (manifest) => {
  manifest.operation_terminal_mapping[0][1] = "InventedDisposition";
});

expectReviewedDrift("released JSONL request spellings are exact", (manifest) => {
  manifest.jsonl_request_inventory[0][0] = "invented/request";
});

expectReviewedDrift("released JSONL event spellings are exact", (manifest) => {
  manifest.jsonl_event_inventory[0][0] = "invented_event";
});

expectReviewedDrift("history wire statuses are exact", (manifest) => {
  manifest.history_statuses[0][1] = "invented_status";
});

expectReviewedDrift("operation invariant strings are exact", (manifest) => {
  manifest.operation_generation_invariants[0] = "placeholder";
});

expectReviewedDrift("Goal invariant strings are exact", (manifest) => {
  manifest.goal_generation_identity_contract.invariants[0] = "placeholder";
});

expectReviewedDrift("history invariant strings are exact", (manifest) => {
  manifest.history_contract_invariants[0] = "placeholder";
});

expectReviewedDrift("repair invariant strings are exact", (manifest) => {
  manifest.deferred_state_nonempty_invariants[0] = "placeholder";
});

expectReviewedDrift("command targets cannot authorize themselves", (manifest) => {
  manifest.closed_inventory.command_targets.push("InventedCommandTarget");
  manifest.thread_commands[0][2] = "InventedCommandTarget";
});

expectReviewedDrift("source scopes cannot authorize themselves", (manifest) => {
  manifest.closed_inventory.source_scopes.push("InventedSourceScope");
  manifest.source_facts[0][5] = "InventedSourceScope";
});

expectReviewedDrift("acknowledgement forms cannot authorize themselves", (manifest) => {
  manifest.closed_inventory.acknowledgement_forms.push("InventedAcknowledgement");
  manifest.thread_commands[0][9].push("InventedAcknowledgement");
});

expectReviewedDrift("deferred values cannot authorize themselves", (manifest) => {
  manifest.closed_inventory.deferred_command_values.push("InventedDeferredValue");
  manifest.thread_commands[0][10].push("InventedDeferredValue");
});

expectReviewedDrift("source ACP dispositions cannot authorize themselves", (manifest) => {
  manifest.adapter_dispositions.acp.push("InventedSourceAcpDisposition");
  manifest.source_facts[0][10] = "InventedSourceAcpDisposition";
});

{
  const manifest = cloneManifest();
  manifest.test_vector_generators[0].source = "missing_inventory";
  expectFailure(
    "test generator sources must resolve",
    () => validateCandidate(manifest),
    /test generator legacy_event_inventory references missing source/,
  );
}

{
  const manifest = cloneManifest();
  manifest.source_facts.pop();
  expectFailure(
    "runtime facts must match EventType",
    () => validateCurrentInventories(manifest, { repoRoot }),
    /source_facts does not match current EventType/,
  );
}

{
  const manifest = cloneManifest();
  manifest.closed_inventory.current_tui_user_actions.pop();
  expectFailure(
    "TUI actions must match UserAction",
    () => validateCurrentInventories(manifest, { repoRoot }),
    /current_tui_user_actions does not match current UserAction/,
  );
}

{
  const manifest = cloneManifest();
  const sessionActions = new Set([
    "ForkCurrentSession",
    "RenameCurrentSession",
    "ResumeSavedSession",
    "ForkSavedSession",
    "RenameSavedSession",
    "ArchiveSavedSession",
    "DeleteSavedSession",
  ]);
  manifest.closed_inventory.current_tui_user_actions =
    manifest.closed_inventory.current_tui_user_actions.filter(
      (action) => !sessionActions.has(action),
    );
  expectFailure(
    "inventory drift lists all seven missing current session actions",
    () => validateCurrentInventories(manifest, { repoRoot }),
    /missing: ForkCurrentSession, RenameCurrentSession, ResumeSavedSession, ForkSavedSession, RenameSavedSession, ArchiveSavedSession, DeleteSavedSession/,
  );
}

{
  const manifest = cloneManifest();
  manifest.tui_entrypoints.pop();
  expectFailure(
    "mutation-capable TUI entrypoints are exact",
    () => validateCurrentInventories(manifest, { repoRoot }),
    /tui_entrypoints does not match the baseline inventory/,
  );
}

{
  const relativePath = "crates/orca-tui/src/slash_command_actions.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const sourceWithCrLf = source.replace(/\r?\n/g, "\r\n");
  const appPath = "crates/orca-tui/src/app.rs";
  const appWithCrLf = readFileSync(path.join(repoRoot, appPath), "utf8").replace(
    /\r?\n/g,
    "\r\n",
  );
  assert.doesNotThrow(
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([
          [relativePath, `// inserted without changing an entrypoint\r\n${sourceWithCrLf}`],
          [appPath, appWithCrLf],
        ]),
      }),
    "entrypoint validation ignores checkout line endings when a reviewed line drifts",
  );
}

{
  const manifest = cloneManifest();
  manifest.tui_entrypoints[0][1] = ["README.md:1"];
  expectFailure(
    "TUI entrypoint sources must stay under the TUI source root",
    () => validateCurrentInventories(manifest, { repoRoot }),
    /outside crates\/orca-tui\/src/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const goalActionCalls = source.match(/\bhandle_hosted_goal_action\s*\(/g) ?? [];
  assert.equal(
    goalActionCalls.length,
    6,
    "goal callback mutation fixture must find all six production dispatch calls",
  );
  const withoutGoalActionDispatch = source.replace(
    /\bhandle_hosted_goal_action\s*\(/g,
    "removed_hosted_goal_action_dispatch(",
  );
  assert.match(
    withoutGoalActionDispatch,
    /handle_hosted_goal_action/,
    "goal callback mutation fixture must preserve the import",
  );
  expectFailure(
    "goal callback validation rejects removed production dispatch even when the import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutGoalActionDispatch]]),
      }),
    /goal_callbacks source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/hosted_controller\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_goal.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutGoalActionOwner = source.replace(
    /pub\(crate\)\s+fn\s+handle_hosted_goal_action\s*\(/,
    "pub(crate) fn removed_hosted_goal_action_owner(",
  );
  assert.notEqual(
    withoutGoalActionOwner,
    source,
    "goal owner mutation fixture must remove the production owner definition",
  );
  assert.match(
    withoutGoalActionOwner,
    /\bhandle_hosted_goal_action\s*\(/,
    "goal owner mutation fixture must preserve a call to the removed owner",
  );
  expectFailure(
    "goal callback validation rejects a removed production owner even when calls remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutGoalActionOwner]]),
      }),
    /goal_callbacks source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/hosted_goal\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const sessionActionCalls = source.match(/\bhandle_hosted_session_action\s*\(/g) ?? [];
  assert.equal(
    sessionActionCalls.length,
    8,
    "session transition mutation fixture must find all eight production dispatch calls",
  );
  const withoutSessionActionDispatch = source.replace(
    /\bhandle_hosted_session_action\s*\(/g,
    "removed_hosted_session_action_dispatch(",
  );
  assert.match(
    withoutSessionActionDispatch,
    /handle_hosted_session_action/,
    "session transition mutation fixture must preserve the import",
  );
  expectFailure(
    "session transition validation rejects removed production dispatch even when the import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutSessionActionDispatch]]),
      }),
    /session_picker_transition source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/hosted_controller\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_session_lifecycle.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutSessionActionOwner = source.replace(
    /pub\(crate\)\s+fn\s+handle_hosted_session_action\s*\(/,
    "pub(crate) fn removed_hosted_session_action_owner(",
  );
  assert.notEqual(
    withoutSessionActionOwner,
    source,
    "session owner mutation fixture must remove the production owner definition",
  );
  assert.match(
    withoutSessionActionOwner,
    /\bhandle_hosted_session_action\s*\(/,
    "session owner mutation fixture must preserve a call to the removed owner",
  );
  expectFailure(
    "session transition validation rejects a removed production owner even when calls remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutSessionActionOwner]]),
      }),
    /session_picker_transition source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/hosted_session_lifecycle\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const productionCaller =
    /TuiAgentRuntime::spawn_hosted\(\s*action_rx,\s*event_tx\.clone\(\),\s*MAX_SUPERVISED_TUI_TASKS,\s*agent_controller,\s*move \|agent_controller, command_rx, host\| \{\s*hosted_tui_controller_loop\s*\(/;
  const withoutProductionCaller = source.replace(productionCaller, (matched) =>
    matched.replace("hosted_tui_controller_loop(", "removed_hosted_tui_controller_loop("),
  );
  assert.notEqual(
    withoutProductionCaller,
    source,
    "controller caller fixture must remove the production controller call",
  );
  assert.match(
    withoutProductionCaller,
    /\bhosted_tui_controller_loop\s*\(/,
    "controller caller fixture must preserve test-harness calls",
  );
  expectFailure(
    "session transition validation rejects a removed production controller caller while tests remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutProductionCaller]]),
      }),
    /session_picker_transition source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/app\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, pattern, replacement] of [
    [
      "StartSideConversation",
      /\bHostedSideAction::Start\s*\{\s*prompt\s*\}/,
      "HostedSideAction::RemovedStart { prompt }",
    ],
    ["ToggleSideConversation", /\bHostedSideAction::Toggle\b/, "HostedSideAction::RemovedToggle"],
    ["CloseSideConversation", /\bHostedSideAction::Close\b/, "HostedSideAction::RemovedClose"],
  ]) {
    const withoutDispatch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutDispatch,
      source,
      `${actionId} mutation fixture must remove its production dispatch`,
    );
    assert.match(
      withoutDispatch,
      /handle_hosted_side_action/,
      `${actionId} mutation fixture must preserve the owner import`,
    );
    expectFailure(
      `${actionId} validation rejects removed production dispatch while the import remains`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutDispatch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_controller\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/hosted_side.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, variant, pattern, replacement] of [
    [
      "StartSideConversation",
      "Start",
      /\bHostedSideAction::Start\s*\{\s*prompt\s*\}\s*=>/,
      "HostedSideAction::RemovedStart { prompt } =>",
    ],
    [
      "ToggleSideConversation",
      "Toggle",
      /\bHostedSideAction::Toggle\s*=>/,
      "HostedSideAction::RemovedToggle =>",
    ],
    [
      "CloseSideConversation",
      "Close",
      /\bHostedSideAction::Close\s*=>/,
      "HostedSideAction::RemovedClose =>",
    ],
  ]) {
    const withoutOwnerBranch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutOwnerBranch,
      source,
      `${actionId} mutation fixture must remove its production owner branch`,
    );
    assert.match(
      withoutOwnerBranch,
      new RegExp(`\\b${variant}\\b`),
      `${actionId} mutation fixture must preserve its enum or test reference`,
    );
    expectFailure(
      `${actionId} validation rejects removed production owner while other references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutOwnerBranch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_side\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, pattern, replacement] of [
    [
      "Remember",
      /\bHostedContextAction::Remember\s*\{\s*scope\s*,\s*note\s*\}/,
      "HostedContextAction::RemovedRemember { scope, note }",
    ],
    ["Compact", /\bHostedContextAction::Compact\b/, "HostedContextAction::RemovedCompact"],
    ["Backtrack", /\bHostedContextAction::Backtrack\b/, "HostedContextAction::RemovedBacktrack"],
  ]) {
    const withoutDispatch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutDispatch,
      source,
      `${actionId} mutation fixture must remove its production dispatch`,
    );
    assert.match(
      withoutDispatch,
      /handle_hosted_context_action/,
      `${actionId} mutation fixture must preserve the owner import`,
    );
    expectFailure(
      `${actionId} validation rejects removed production dispatch while the import remains`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutDispatch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_controller\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/hosted_context.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, variant, pattern, replacement] of [
    [
      "Remember",
      "Remember",
      /\bHostedContextAction::Remember\s*\{\s*scope\s*,\s*note\s*\}\s*=>/,
      "HostedContextAction::RemovedRemember { scope, note } =>",
    ],
    [
      "Compact",
      "Compact",
      /\bHostedContextAction::Compact\s*=>/,
      "HostedContextAction::RemovedCompact =>",
    ],
    [
      "Backtrack",
      "Backtrack",
      /\bHostedContextAction::Backtrack\s*=>/,
      "HostedContextAction::RemovedBacktrack =>",
    ],
  ]) {
    const withoutOwnerBranch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutOwnerBranch,
      source,
      `${actionId} mutation fixture must remove its production owner branch`,
    );
    assert.match(
      withoutOwnerBranch,
      new RegExp(`\\b${variant}\\b`),
      `${actionId} mutation fixture must preserve its enum or test reference`,
    );
    expectFailure(
      `${actionId} validation rejects removed production owner while other references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutOwnerBranch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_context\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutDispatch = source.replace(
    /\bHostedWorkflowAction::Run\s*\{\s*name\s*,\s*args\s*\}/,
    "HostedWorkflowAction::RemovedRun { name, args }",
  );
  assert.notEqual(withoutDispatch, source, "RunWorkflow fixture must remove its production dispatch");
  assert.match(
    withoutDispatch,
    /handle_hosted_workflow_action/,
    "RunWorkflow fixture must preserve the owner import",
  );
  expectFailure(
    "RunWorkflow validation rejects removed production dispatch while the import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutDispatch]]),
      }),
    /RunWorkflow source does not contain its reviewed action anchor: crates\/orca-tui\/src\/hosted_controller\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_workflow.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutOwnerBranch = source.replace(
    /\bHostedWorkflowAction::Run\s*\{\s*name\s*,\s*args\s*\}\s*=>/,
    "HostedWorkflowAction::RemovedRun { name, args } =>",
  );
  assert.notEqual(
    withoutOwnerBranch,
    source,
    "RunWorkflow fixture must remove its production owner branch",
  );
  assert.match(
    withoutOwnerBranch,
    /\bRun\b/,
    "RunWorkflow fixture must preserve an enum or test reference",
  );
  expectFailure(
    "RunWorkflow validation rejects removed production owner while other references remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutOwnerBranch]]),
      }),
    /RunWorkflow source does not contain its reviewed action anchor: crates\/orca-tui\/src\/hosted_workflow\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, pattern, replacement] of [
    [
      "ResumeOperation",
      /\bHostedOperationAction::Resume\s*\{\s*operation_id\s*\}/,
      "HostedOperationAction::RemovedResume { operation_id }",
    ],
    [
      "CancelOperation",
      /\bHostedOperationAction::Cancel\s*\{\s*operation_id\s*\}/,
      "HostedOperationAction::RemovedCancel { operation_id }",
    ],
  ]) {
    const withoutDispatch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutDispatch,
      source,
      `${actionId} fixture must remove its production dispatch`,
    );
    assert.match(
      withoutDispatch,
      /handle_hosted_operation_action/,
      `${actionId} fixture must preserve the owner import`,
    );
    expectFailure(
      `${actionId} validation rejects removed production dispatch while the import remains`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutDispatch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_controller\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/hosted_operation.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, variant, pattern, replacement] of [
    [
      "ResumeOperation",
      "Resume",
      /\bHostedOperationAction::Resume\s*\{\s*operation_id\s*\}\s*=>/,
      "HostedOperationAction::RemovedResume { operation_id } =>",
    ],
    [
      "CancelOperation",
      "Cancel",
      /\bHostedOperationAction::Cancel\s*\{\s*operation_id\s*\}\s*=>/,
      "HostedOperationAction::RemovedCancel { operation_id } =>",
    ],
  ]) {
    const withoutOwnerBranch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutOwnerBranch,
      source,
      `${actionId} fixture must remove its production owner branch`,
    );
    assert.match(
      withoutOwnerBranch,
      new RegExp(`HostedOperationAction::${variant}`),
      `${actionId} fixture must preserve its enum or test reference`,
    );
    expectFailure(
      `${actionId} validation rejects removed production owner while other references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutOwnerBranch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_operation\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutDispatch = source.replace(
    /\bHostedPlanAction::ImplementApproved\s*\{\s*prompt\s*,\s*approval_mode\s*,?\s*\}/,
    "HostedPlanAction::RemovedImplementApproved { prompt, approval_mode }",
  );
  assert.notEqual(
    withoutDispatch,
    source,
    "ImplementApprovedPlan fixture must remove its production dispatch",
  );
  assert.match(
    withoutDispatch,
    /handle_hosted_plan_action/,
    "ImplementApprovedPlan fixture must preserve the owner import",
  );
  expectFailure(
    "ImplementApprovedPlan validation rejects removed dispatch while the import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutDispatch]]),
      }),
    /ImplementApprovedPlan source does not contain its reviewed action anchor: crates\/orca-tui\/src\/hosted_controller\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_plan.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutOwnerBranch = source.replace(
    /\bHostedPlanAction::ImplementApproved\s*\{\s*prompt\s*,\s*approval_mode\s*,?\s*\}\s*=>/,
    "HostedPlanAction::RemovedImplementApproved { prompt, approval_mode } =>",
  );
  assert.notEqual(
    withoutOwnerBranch,
    source,
    "ImplementApprovedPlan fixture must remove its production owner branch",
  );
  assert.match(
    withoutOwnerBranch,
    /HostedPlanAction::ImplementApproved/,
    "ImplementApprovedPlan fixture must preserve its enum or test reference",
  );
  expectFailure(
    "ImplementApprovedPlan validation rejects removed owner while other references remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutOwnerBranch]]),
      }),
    /ImplementApprovedPlan source does not contain its reviewed action anchor: crates\/orca-tui\/src\/hosted_plan\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/hosted_controller.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, pattern, replacement] of [
    [
      "ResolveBackgroundApproval",
      /\bHostedTaskAction::ResolveBackgroundApproval\s*\{\s*id\s*,\s*approved\s*,?\s*\}/,
      "HostedTaskAction::RemovedResolveBackgroundApproval { id, approved }",
    ],
    [
      "StopTask",
      /\bHostedTaskAction::Stop\s*\{\s*task_id\s*\}/,
      "HostedTaskAction::RemovedStop { task_id }",
    ],
    [
      "ForegroundTask",
      /\bHostedTaskAction::Foreground\s*\{\s*task_id\s*\}/,
      "HostedTaskAction::RemovedForeground { task_id }",
    ],
  ]) {
    const withoutDispatch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutDispatch,
      source,
      `${actionId} fixture must remove its production dispatch`,
    );
    assert.match(
      withoutDispatch,
      /handle_hosted_task_action/,
      `${actionId} fixture must preserve the owner import`,
    );
    expectFailure(
      `${actionId} validation rejects removed production dispatch while the import remains`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutDispatch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/hosted_controller\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/background_tasks.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [actionId, variant, pattern, replacement] of [
    [
      "ResolveBackgroundApproval",
      "ResolveBackgroundApproval",
      /\bHostedTaskAction::ResolveBackgroundApproval\s*\{\s*id\s*,\s*approved\s*,?\s*\}\s*=>/,
      "HostedTaskAction::RemovedResolveBackgroundApproval { id, approved } =>",
    ],
    [
      "StopTask",
      "Stop",
      /\bHostedTaskAction::Stop\s*\{\s*task_id\s*\}\s*=>/,
      "HostedTaskAction::RemovedStop { task_id } =>",
    ],
    [
      "ForegroundTask",
      "Foreground",
      /\bHostedTaskAction::Foreground\s*\{\s*task_id\s*\}\s*=>/,
      "HostedTaskAction::RemovedForeground { task_id } =>",
    ],
  ]) {
    const withoutOwnerBranch = source.replace(pattern, replacement);
    assert.notEqual(
      withoutOwnerBranch,
      source,
      `${actionId} fixture must remove its production owner branch`,
    );
    assert.match(
      withoutOwnerBranch,
      new RegExp(`HostedTaskAction::${variant}`),
      `${actionId} fixture must preserve its enum or test reference`,
    );
    expectFailure(
      `${actionId} validation rejects removed production owner while other references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: new Map([[relativePath, withoutOwnerBranch]]),
        }),
      new RegExp(
        `${actionId} source does not contain its reviewed action anchor: crates\\/orca-tui\\/src\\/background_tasks\\.rs`,
      ),
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/idle_submit_actions.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const pattern =
    /\blet\s+_\s*=\s*action_tx\.send\(UserAction::RespondToInteraction\s*\{\s*key\s*,\s*response\s*\}\);/;
  const withoutPendingInputDispatch = source.replace(
    pattern,
    "let _ = action_tx.send(UserAction::RemovedRespondToInteraction { key, response });",
  );
  assert.notEqual(
    withoutPendingInputDispatch,
    source,
    "RespondToInteraction fixture must remove its pending-input production dispatch",
  );
  assert.match(
    withoutPendingInputDispatch,
    /RespondToInteraction/,
    "RespondToInteraction fixture must preserve its tests and other references",
  );
  expectFailure(
    "RespondToInteraction validation rejects removed pending-input dispatch while tests remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutPendingInputDispatch]]),
      }),
    /RespondToInteraction source does not contain its reviewed action anchor: crates\/orca-tui\/src\/idle_submit_actions\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/action_dispatcher.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const pattern =
    /\bUserAction::RespondToInteraction\s*\{\s*key\s*,\s*response\s*\}\s*=>/;
  const withoutPrioritizedOwner = source.replace(
    pattern,
    "UserAction::RemovedRespondToInteraction { key, response } =>",
  );
  assert.notEqual(
    withoutPrioritizedOwner,
    source,
    "RespondToInteraction fixture must remove its prioritized owner branch",
  );
  assert.match(
    withoutPrioritizedOwner,
    /RespondToInteraction/,
    "RespondToInteraction fixture must preserve its tests and other references",
  );
  expectFailure(
    "RespondToInteraction validation rejects removed prioritized owner while tests remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutPrioritizedOwner]]),
      }),
    /RespondToInteraction source does not contain its reviewed action anchor: crates\/orca-tui\/src\/action_dispatcher\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const pattern =
    /for\s+ack\s+in\s+interaction_ack_rx\.try_iter\(\)\s*\{\s*handle_interaction_response_ack\s*\(/;
  const withoutInteractionAckDrain = source.replace(
    pattern,
    "for ack in interaction_ack_rx.try_iter() { handle_removed_interaction_response_ack(",
  );
  assert.notEqual(
    withoutInteractionAckDrain,
    source,
    "RespondToInteraction fixture must remove its production acknowledgement drain",
  );
  assert.match(
    withoutInteractionAckDrain,
    /RespondToInteraction/,
    "RespondToInteraction fixture must preserve its tests and other references",
  );
  expectFailure(
    "RespondToInteraction validation rejects removed acknowledgement drain while tests remain",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: new Map([[relativePath, withoutInteractionAckDrain]]),
      }),
    /RespondToInteraction source does not contain its reviewed action anchor: crates\/orca-tui\/src\/app\.rs/,
  );
}

{
  const manifest = cloneManifest();
  const appPath = path.join(repoRoot, "crates/orca-tui/src/app.rs");
  const syntheticApp = `${readFileSync(appPath, "utf8")}\n\
fn synthetic_surface_mutation(runtime_thread: &RuntimeThreadHandle) {\n\
    let _ = runtime_thread.mutate(RuntimeThreadMutation::SetModel(None));\n\
}\n`;
  expectFailure(
    "unlisted mutation-capable TUI call sites are rejected",
    () =>
      validateCurrentInventories(manifest, {
        repoRoot,
        sourceOverrides: new Map([["crates/orca-tui/src/app.rs", syntheticApp]]),
      }),
    /unlisted mutation-capable TUI entrypoint synthetic_surface_mutation/,
  );
}

for (const [label, functionName, body] of [
  [
    "UFCS RuntimeThreadHandle::mutate is detected",
    "synthetic_ufcs_mutate",
    "RuntimeThreadHandle::mutate(runtime_thread, RuntimeThreadMutation::SetModel(None));",
  ],
  ["RuntimeThreadHandle::start_turn is detected", "synthetic_start_turn", "thread.start_turn(request, sink);"],
  [
    "RuntimeThreadHandle::start_turn_with_config is detected",
    "synthetic_start_turn_with_config",
    "thread.start_turn_with_config(request, sink, config);",
  ],
  ["runtime thread shutdown is detected", "synthetic_thread_shutdown", "runtime_thread.shutdown();"],
  ["RuntimeHost::shutdown UFCS is detected", "synthetic_host_shutdown", "RuntimeHost::shutdown(host);"],
  ["workflow launch is detected", "synthetic_launch_workflow", "runtime_thread.launch_workflow(request);"],
  [
    "backtrack mutation is detected",
    "synthetic_backtrack",
    "RuntimeThreadHandle::backtrack_last_user(runtime_thread);",
  ],
  ["host thread creation is detected", "synthetic_start_thread", "host.start_thread_with_request(request);"],
]) {
  expectUnlistedRuntimeMutation(label, functionName, body);
}

for (const [label, source] of [
  [
    "type aliases cannot evade associated shutdown detection",
    `type RuntimeAlias = RuntimeThreadHandle;
fn synthetic_type_alias_shutdown() { RuntimeAlias::shutdown(runtime_thread); }`,
  ],
  [
    "import aliases cannot evade associated shutdown detection",
    `use orca_runtime::RuntimeThreadHandle as RuntimeAlias;
fn synthetic_import_alias_shutdown() { RuntimeAlias::shutdown(runtime_thread); }`,
  ],
  [
    "associated function items cannot evade shutdown detection",
    `fn synthetic_function_item_shutdown() { let stop = RuntimeThreadHandle::shutdown; stop(runtime_thread); }`,
  ],
  [
    "qualified associated function items cannot evade shutdown detection",
    `fn synthetic_qualified_function_item_shutdown() { let stop = <RuntimeThreadHandle>::shutdown; stop(runtime_thread); }`,
  ],
  [
    "trait-qualified associated function items cannot evade shutdown detection",
    `fn synthetic_trait_qualified_function_item_shutdown() { let stop = <RuntimeThreadHandle as RuntimeThreadOps>::shutdown; stop(runtime_thread); }`,
  ],
  [
    "multiline associated function paths cannot evade shutdown detection",
    `fn synthetic_multiline_associated_shutdown() {
  <orca_runtime::
    RuntimeThreadHandle>::
    shutdown(runtime_thread);
}`,
  ],
]) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(source),
      }),
    /unlisted mutation-capable TUI entrypoint synthetic_/,
  );
}

for (const [label, source] of [
  [
    "same-line import aliases cannot suppress associated scanning",
    `use orca_runtime::RuntimeThreadHandle as RuntimeAlias; fn synthetic_same_line_alias() { RuntimeAlias::shutdown(runtime_thread); }`,
  ],
  [
    "same-line imports cannot suppress direct associated items",
    `use std::sync::Arc; fn synthetic_same_line_direct() { let stop = RuntimeThreadHandle::shutdown; stop(runtime_thread); }`,
  ],
  [
    "multiline use groups mask only the declaration span",
    `use std::{
  collections::HashMap,
  sync::Arc,
}; fn synthetic_multiline_use_group() { let stop = RuntimeThreadHandle::shutdown; stop(runtime_thread); }`,
  ],
  [
    "masked comments and strings do not extend use declaration spans",
    `use std::sync::Arc; /* use bogus::RuntimeThreadHandle; */ fn synthetic_use_comment_edge() { let text = "use bogus::RuntimeThreadHandle;"; let stop = RuntimeThreadHandle::shutdown; stop(runtime_thread); }`,
  ],
]) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(source),
      }),
    /unlisted mutation-capable TUI entrypoint synthetic_/,
  );
}

expectFailure(
  "raw use identifiers cannot be mistaken for use declarations",
  () =>
    validateCurrentInventories(cloneManifest(), {
      repoRoot,
      sourceOverrides: appSourceOverride(
        `fn probe_raw_use() { let r#use = RuntimeThreadHandle::shutdown; r#use(runtime_thread); }`,
      ),
    }),
  /unlisted mutation-capable TUI entrypoint probe_raw_use/,
);

for (const [label, source] of [
  [
    "comments and whitespace before true use items preserve later tokens",
    `pub /* visibility comment */ use std::sync::Arc; fn synthetic_commented_true_use() { let stop = RuntimeThreadHandle::shutdown; stop(runtime_thread); }`,
  ],
  [
    "other raw identifiers remain visible to associated scanning",
    `fn synthetic_other_raw_identifier() { let r#user = RuntimeThreadHandle::shutdown; r#user(runtime_thread); }`,
  ],
]) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(source),
      }),
    /unlisted mutation-capable TUI entrypoint synthetic_/,
  );
}

for (const [label, functionName, item] of [
  ["policy function items cannot evade detection", "synthetic_policy_function_item", "folder_trust::set_trust"],
  ["memory function items cannot evade detection", "synthetic_memory_function_item", "orca_runtime::memory::remember_user"],
  ["credential function items cannot evade detection", "synthetic_credential_function_item", "crate::save_api_key"],
  ["Goal helper function items cannot evade detection", "synthetic_goal_helper_function_item", "crate::update_goal_status_for_session"],
  ["catalog function items cannot evade detection", "synthetic_catalog_function_item", "orca_mcp::initialize_registry"],
]) {
  expectUnlistedRuntimeMutation(label, functionName, `let authority = ${item};`);
}

expectFailure(
  "unknown generic associated function items fail closed",
  () =>
    validateCurrentInventories(cloneManifest(), {
      repoRoot,
      sourceOverrides: appSourceOverride(
        `fn synthetic_unknown_generic_associated_shutdown<T>(value: T) { let stop = T::shutdown; stop(value); }`,
      ),
    }),
  /unclassified associated TUI function item synthetic_unknown_generic_associated_shutdown/,
);

for (const [label, source] of [
  [
    "free authority function import aliases retain authority",
    `use folder_trust::set_trust as update_trust;
fn synthetic_authority_function_import_alias() { update_trust(&cwd, TrustLevel::Trusted); }`,
  ],
  [
    "qualified UserAction paths retain routing authority",
    `fn synthetic_qualified_user_action() { action_tx.send(crate::types::UserAction::Cancel); }`,
  ],
  [
    "UserAction parameters retain routing authority",
    `fn synthetic_user_action_parameter(action_tx: Sender<UserAction>, action: UserAction) { action_tx.send(action); }`,
  ],
  [
    "UserAction type import aliases retain routing authority",
    `use crate::types::UserAction as Action;
fn synthetic_user_action_type_alias() { action_tx.send(Action::Cancel); }`,
  ],
  [
    "UserAction variant import aliases retain routing authority",
    `use crate::types::UserAction::Cancel as Stop;
fn synthetic_user_action_variant_alias() { action_tx.send(Stop); }`,
  ],
  [
    "UserAction typed locals retain routing authority",
    `fn synthetic_user_action_typed_local() { let action: UserAction = UserAction::Cancel; action_tx.send(action); }`,
  ],
  [
    "UserAction assignment retains routing authority",
    `fn synthetic_user_action_assignment() { let mut action; action = UserAction::Cancel; action_tx.send(action); }`,
  ],
  [
    "UserAction alias chains retain routing authority",
    `fn synthetic_user_action_alias_chain() { let first = UserAction::Cancel; let action = first; action_tx.send(action); }`,
  ],
  [
    "imported authority function items retain authority through rebinding",
    `use folder_trust::set_trust as update_trust;
fn synthetic_authority_function_rebinding() { let apply = update_trust; apply(&cwd, TrustLevel::Trusted); }`,
  ],
]) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(source),
      }),
    /unlisted mutation-capable TUI entrypoint synthetic_/,
  );
}

expectFailure(
  "unresolved values sent through an action sender fail closed",
  () =>
    validateCurrentInventories(cloneManifest(), {
      repoRoot,
      sourceOverrides: appSourceOverride(
        `fn synthetic_unresolved_user_action_send() { action_tx.send(possibly_action); }`,
      ),
    }),
  /unresolved possible UserAction send synthetic_unresolved_user_action_send/,
);

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutOwnerCall = source.replace(
    /\brenderer_runtime\.handle\s*\(/,
    "removed_renderer_runtime.handle(",
  );
  assert.notEqual(
    withoutOwnerCall,
    source,
    "renderer runtime fixture must remove the production app caller",
  );
  assert.match(
    withoutOwnerCall,
    /RendererRuntimeEventOwner/,
    "renderer runtime fixture must preserve the owner import and construction",
  );
  expectFailure(
    "renderer runtime validation rejects a removed app caller while the owner import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: sourceOverride(relativePath, withoutOwnerCall),
      }),
    /renderer_runtime_events source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/app\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/renderer_runtime.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [label, pattern, replacement, preserved] of [
    [
      "attachment admission",
      /match\s+accept_attached_tui_event\s*\(/,
      "match removed_accept_attached_tui_event(",
      /use crate::attachment_routing::accept_attached_tui_event/,
    ],
    [
      "deferred prompt consumption",
      /self\.pending_initial_prompt\.take\(\)/,
      "self.pending_initial_prompt.removed_take()",
      /pending_initial_prompt/,
    ],
    [
      "general reducer delegation",
      /tui_event\s*=>\s*\{\s*handle_runtime_event\s*\(/,
      "tui_event => { removed_handle_runtime_event(",
      /use crate::runtime_event_actions::handle_runtime_event/,
    ],
  ]) {
    const withoutProductionPath = source.replace(pattern, replacement);
    assert.notEqual(
      withoutProductionPath,
      source,
      `${label} fixture must remove its production path`,
    );
    assert.match(
      withoutProductionPath,
      preserved,
      `${label} fixture must preserve a masking import, field, or test reference`,
    );
    expectFailure(
      `renderer runtime validation rejects removed ${label} while other references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: sourceOverride(relativePath, withoutProductionPath),
        }),
      /renderer_runtime_events source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_runtime\.rs/,
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutOwnerCall = source.replace(
    /\brenderer_frame\.prepare_iteration\s*\(/,
    "renderer_frame.removed_prepare_iteration(",
  );
  assert.notEqual(
    withoutOwnerCall,
    source,
    "renderer frame fixture must remove the production app caller",
  );
  assert.match(
    withoutOwnerCall,
    /RendererFrameOwner/,
    "renderer frame fixture must preserve the owner import and construction",
  );
  expectFailure(
    "renderer frame validation rejects a removed app caller while the owner import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: sourceOverride(relativePath, withoutOwnerCall),
      }),
    /renderer_frame source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/app\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/renderer_frame.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [label, pattern, replacement, preserved, expected] of [
    [
      "iteration preparation",
      /state\.poll_edit_highlight_results\(\)/,
      "state.removed_poll_edit_highlight_results()",
      /fn\s+prepare_iteration/,
      /renderer_frame source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_frame\.rs/,
    ],
    [
      "clipboard consumption",
      /state\.pending_clipboard_copy\.take\(\)/,
      "state.pending_clipboard_copy.removed_take()",
      /pending_clipboard_copy/,
      /(?:terminal_clipboard_notifications|renderer_frame) source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_frame\.rs/,
    ],
    [
      "pending presentation output",
      /write_pending\(terminal,\s*presentation,\s*state\.status\);/,
      "removed_pending_output(terminal, presentation, state.status);",
      /WritePending/,
      /renderer_frame source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_frame\.rs/,
    ],
    [
      "terminal draw",
      /terminal\.draw\(/,
      "terminal.removed_draw(",
      /\.draw\(/,
      /renderer_frame source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_frame\.rs/,
    ],
    [
      "successful draw acknowledgement",
      /self\.scheduler\.did_draw\(draw_at\);/,
      "self.scheduler.removed_did_draw(draw_at);",
      /scheduler\.did_draw\(initial_draw_at\)/,
      /renderer_frame source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_frame\.rs/,
    ],
  ]) {
    const withoutProductionPath = source.replace(pattern, replacement);
    assert.notEqual(
      withoutProductionPath,
      source,
      `${label} fixture must remove its production path`,
    );
    assert.match(
      withoutProductionPath,
      preserved,
      `${label} fixture must preserve a masking import, parameter, constructor, or test reference`,
    );
    expectFailure(
      `renderer frame validation rejects removed ${label} while masking references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: sourceOverride(relativePath, withoutProductionPath),
        }),
      expected,
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [label, pattern, replacement, preserved] of [
    [
      "pending owner construction",
      /PendingTerminalSession::start\(/,
      "PendingTerminalSession::removed_start(",
      /use\s+crate::terminal_session::PendingTerminalSession/,
    ],
    [
      "agent startup failure route",
      /pending_terminal_session\.fail_after_agent_startup\(error\)/,
      "pending_terminal_session.removed_agent_failure_route(error)",
      /pending_terminal_session\.activate\(\)/,
    ],
    [
      "terminal activation",
      /pending_terminal_session\.activate\(\)/,
      "pending_terminal_session.removed_activate()",
      /PendingTerminalSession/,
    ],
  ]) {
    const withoutProductionPath = source.replace(pattern, replacement);
    assert.notEqual(
      withoutProductionPath,
      source,
      `${label} fixture must remove its production app path`,
    );
    assert.match(
      withoutProductionPath,
      preserved,
      `${label} fixture must preserve a masking owner reference`,
    );
    expectFailure(
      `terminal session startup validation rejects removed ${label} while owner references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: sourceOverride(relativePath, withoutProductionPath),
        }),
      /terminal_session_startup source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/app\.rs/,
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/terminal_session.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [label, pattern, replacement, preserved] of [
    [
      "production input start",
      /InputRuntime::start\(/,
      "InputRuntime::removed_start(",
      /InputRuntime::finish/,
    ],
    [
      "agent failure finish delegation",
      /finish_startup_failure_with\(&mut self\.input_runtime,\s*error,\s*InputRuntime::finish\)/,
      "removed_finish_route(&mut self.input_runtime, error, InputRuntime::finish)",
      /finish_startup_failure_with::<\(\), _>/,
    ],
    [
      "terminal construction",
      /InlineTerminal::new,/,
      "InlineTerminal::removed_new,",
      /activate_terminal_session_with\(/,
    ],
    [
      "startup clear",
      /InlineTerminal::clear,/,
      "InlineTerminal::removed_clear,",
      /clear_calls/,
    ],
  ]) {
    const withoutProductionPath = source.replace(pattern, replacement);
    assert.notEqual(
      withoutProductionPath,
      source,
      `${label} fixture must remove its production owner path`,
    );
    assert.match(
      withoutProductionPath,
      preserved,
      `${label} fixture must preserve a masking type, helper, or test reference`,
    );
    expectFailure(
      `terminal session startup validation rejects removed ${label} while masking references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: sourceOverride(relativePath, withoutProductionPath),
        }),
      /terminal_session_startup source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/terminal_session\.rs/,
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutOwnerCall = source.replace(
    /renderer_input_wake\.receive\s*\(/,
    "renderer_input_wake.removed_receive(",
  );
  assert.notEqual(
    withoutOwnerCall,
    source,
    "renderer input wake fixture must remove the production app caller",
  );
  assert.match(
    withoutOwnerCall,
    /RendererInputWakeOwner::new/,
    "renderer input wake fixture must preserve owner construction",
  );
  expectFailure(
    "renderer input wake validation rejects a removed app caller while owner construction remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: sourceOverride(relativePath, withoutOwnerCall),
      }),
    /renderer_input_wake source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/app\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/renderer_input_wake.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [label, pattern, replacement, preserved] of [
    [
      "receiver transfer",
      /receivers\.into_parts\(\)/,
      "receivers.removed_into_parts()",
      /pub\(crate\)\s+fn\s+new/,
    ],
    [
      "priority selection",
      /receive_prioritized_input_or_control\(/,
      "removed_priority_selection(",
      /pub\(crate\)\s+fn\s+receive/,
    ],
    [
      "motion filtering",
      /filter\(should_queue_input_event\)/,
      "removed_motion_filter()",
      /should_queue_input_event/,
    ],
    [
      "first suspend acknowledgement",
      /acknowledge\.send\(\(\)\)\.map_err\(/,
      "removed_first_ack.map_err(",
      /acknowledged\.blocking_recv/,
    ],
    [
      "resumed callback",
      /Ok\(InputControl::Resumed\)\s*=>\s*\{\s*resume\(\)\?;/,
      "Ok(InputControl::Resumed) => { removed_resume();",
      /resume failed/,
    ],
    [
      "repeated suspend acknowledgement",
      /Ok\(InputControl::Suspend\s*\{\s*acknowledge\s*\}\)\s*=>\s*\{\s*let _ = acknowledge\.send\(\(\)\);/,
      "Ok(InputControl::Suspend { acknowledge }) => { removed_repeat_ack();",
      /InputControl::Suspend \{/,
    ],
    [
      "suspended disconnect",
      /terminal input runtime disconnected while suspended/,
      "removed_suspended_disconnect",
      /suspended_control_disconnect_keeps_exact_error/,
    ],
    [
      "ordinary disconnect",
      /terminal input runtime disconnected"/,
      "removed ordinary disconnect",
      /disconnected_control_wake_keeps_exact_error/,
    ],
  ]) {
    const withoutProductionPath = source.replace(pattern, replacement);
    assert.notEqual(
      withoutProductionPath,
      source,
      `${label} fixture must remove its production owner path`,
    );
    assert.match(
      withoutProductionPath,
      preserved,
      `${label} fixture must preserve a masking owner or test reference`,
    );
    expectFailure(
      `renderer input wake validation rejects removed ${label} while masking references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: sourceOverride(relativePath, withoutProductionPath),
        }),
      /renderer_input_wake source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_input_wake\.rs/,
    );
  }
}

{
  const relativePath = "crates/orca-tui/src/app.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const withoutOwnerCall = source.replace(
    /return\s+RendererInputRouter::new\s*\(/,
    "return RemovedRendererInputRouter::new(",
  );
  assert.notEqual(
    withoutOwnerCall,
    source,
    "renderer input routing fixture must remove the production app delegation",
  );
  assert.match(
    withoutOwnerCall,
    /use\s+crate::renderer_input_router::RendererInputRouter;/,
    "renderer input routing fixture must preserve the masking owner import",
  );
  expectFailure(
    "renderer input routing validation rejects removed app delegation while the import remains",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: sourceOverride(relativePath, withoutOwnerCall),
      }),
    /renderer_input_routing source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/app\.rs/,
  );
}

{
  const relativePath = "crates/orca-tui/src/renderer_input_router.rs";
  const source = readFileSync(path.join(repoRoot, relativePath), "utf8");
  for (const [label, pattern, replacement, preserved] of [
    [
      "scroll flush order",
      /handle_scroll_lines\(self\.state,\s*lines,\s*now\);/,
      "removed_scroll(self.state, lines, now);",
      /scroll_flushes_insert_escape_and_cancels_pending_vim_command_first/,
    ],
    [
      "focus short circuit",
      /if\s+consume_focus_event\(&event,\s*self\.presentation\)\s*\{/,
      "if removed_focus_event(&event, self.presentation) {",
      /focus_is_consumed_before_other_semantic_routing/,
    ],
    [
      "insert escape resolution",
      /if\s+resolve_pending_insert_escape_before_routing\(\s*&event,/,
      "if removed_insert_escape_resolution(\n                    &event,",
      /resolve_pending_insert_escape_before_routing/,
    ],
    [
      "paste preflush",
      /if\s+matches!\(event,\s*Event::Paste\(_\)\)\s*\{\s*flush_pending_insert_escape_before_non_key\(/,
      "if matches!(event, Event::Paste(_)) {\n                    removed_paste_preflush(",
      /paste_flushes_insert_escape_before_paste_ownership/,
    ],
    [
      "resize short circuit",
      /if\s+handle_resize_event\(&event,\s*self\.state\)\s*\{/,
      "if removed_resize_event(&event, self.state) {",
      /resize_invalidates_selection_without_key_fallthrough/,
    ],
    [
      "mouse preflush",
      /if\s+matches!\(event,\s*Event::Mouse\(_\)\)\s*\{\s*flush_pending_insert_escape_before_non_key\(/,
      "if matches!(event, Event::Mouse(_)) {\n                    removed_mouse_preflush(",
      /mouse_confirmation_dispatches_the_selected_plan_action/,
    ],
    [
      "handled mouse cancellation",
      /MouseFlow::Handled\s*=>\s*\{\s*self\.vim_state\.cancel_pending_command\(\);/,
      "MouseFlow::Handled => { removed_mouse_cancel();",
      /MouseFlow::Handled/,
    ],
    [
      "synthetic Enter direct status routing",
      /return\s+self\.route_status_key\(&event,\s*&key,\s*&mut\s+clear_terminal\);/,
      "return removed_synthetic_status(&event, &key, &mut clear_terminal);",
      /KeyEvent::new\(KeyCode::Enter,\s*KeyModifiers::NONE\)/,
    ],
    [
      "real key preflight",
      /match\s+handle_key_event_preflight\(\s*\*key,/,
      "match removed_key_preflight(\n                    *key,",
      /handle_key_event_preflight/,
    ],
    [
      "preflight exit folding",
      /KeyEventFlow::Exit\(code\)\s*=>\s*return\s+Ok\(Some\(code\)\),/,
      "KeyEventFlow::Exit(code) => return removed_preflight_exit(code),",
      /KeyEventFlow::Exit/,
    ],
    [
      "status exit folding",
      /StatusKeyFlow::Exit\(code\)\s*=>\s*Ok\(Some\(code\)\),/,
      "StatusKeyFlow::Exit(code) => removed_status_exit(code),",
      /StatusKeyFlow::Exit/,
    ],
  ]) {
    const withoutProductionPath = source.replace(pattern, replacement);
    assert.notEqual(
      withoutProductionPath,
      source,
      `${label} fixture must remove its production owner path`,
    );
    assert.match(
      withoutProductionPath,
      preserved,
      `${label} fixture must preserve a masking import, branch, or owner test`,
    );
    expectFailure(
      `renderer input routing validation rejects removed ${label} while masking references remain`,
      () =>
        validateCurrentInventories(cloneManifest(), {
          repoRoot,
          sourceOverrides: sourceOverride(relativePath, withoutProductionPath),
        }),
      /renderer_input_routing source does not contain its reviewed entrypoint anchor: crates\/orca-tui\/src\/renderer_input_router\.rs/,
    );
  }
}

for (const [label, functionName, parameter, body] of [
  [
    "operation controller shutdown retains runtime authority",
    "synthetic_controller_shutdown",
    "controller: &TuiOperationController",
    "controller.shutdown();",
  ],
  [
    "action dispatcher shutdown retains controller authority",
    "synthetic_dispatcher_shutdown",
    "dispatcher: &mut TuiActionDispatcher",
    "dispatcher.shutdown();",
  ],
  [
    "agent runtime shutdown retains host authority",
    "synthetic_agent_runtime_shutdown",
    "agent_runtime: &mut TuiAgentRuntime",
    "agent_runtime.shutdown();",
  ],
  [
    "pending interaction store insert retains projection authority",
    "synthetic_pending_interaction_insert",
    "store: &RuntimePendingInteractionStore",
    "store.insert(record);",
  ],
]) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(
          `fn ${functionName}(${parameter}) { ${body} }`,
        ),
      }),
    new RegExp(`unlisted mutation-capable TUI entrypoint ${functionName}`),
  );
}

{
  const functionName = "synthetic_broker_pending_insert";
  expectFailure(
    "broker state aliases retain interaction authority",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(
          `impl TuiInteractionBroker { fn ${functionName}(&self) { let mut state = self.lock_state(); state.pending.insert(key, pending); } }`,
        ),
      }),
    new RegExp(`unlisted mutation-capable TUI entrypoint ${functionName}`),
  );
}

for (const [family, functionName, body] of [
  ["settings mutation", "synthetic_settings_mutation", "cycle_approval_mode(config, shared_config, state);"],
  ["policy store mutation", "synthetic_policy_mutation", "folder_trust::set_trust(&cwd, TrustLevel::Trusted);"],
  ["memory store mutation", "synthetic_memory_mutation", "orca_runtime::memory::remember_user(&note);"],
  ["credential store mutation", "synthetic_credential_mutation", "save_api_key(&key);"],
  ["UserAction mutation routing", "synthetic_user_action_route", "action_tx.send(UserAction::GoalPause);"],
  ["operation handle control", "synthetic_operation_interrupt", "operation.interrupt();"],
  ["operation UFCS control", "synthetic_operation_ufcs_interrupt", "OperationHandle::interrupt(operation);"],
  ["controller control", "synthetic_controller_interrupt", "controller.interrupt_current();"],
  ["interaction broker response", "synthetic_broker_respond", "controller.broker().respond(&key, response);"],
  ["interaction registration", "synthetic_interaction_register", "control.register_interaction(kind, request_id);"],
  ["Goal pause mutation", "synthetic_goal_pause", "runtime.pause(session_id, revision, now);"],
  ["Goal resume mutation", "synthetic_goal_resume", "runtime.resume(session_id, revision, now);"],
  ["Goal status helper", "synthetic_goal_status", "update_goal_status_for_session(thread, session, status, event_tx);"],
  ["workflow continuation", "synthetic_workflow_continuation", "submit_pending_workflow_notification(state, action_tx, true);"],
  ["task stop mutation", "synthetic_task_stop", "stop_task_for_tui(registry, task_id, event_tx);"],
  ["task foreground mutation", "synthetic_task_foreground", "foreground_task_for_tui(registry, task_id, event_tx);"],
  ["background approval mutation", "synthetic_background_approval", "submit_background_approval_response_for_tui(registry, id, approved, event_tx);"],
  ["session transition", "synthetic_session_transition", "resume_selected_session(state, config, shared, preloaded, clear);"],
  ["catalog mutation", "synthetic_catalog_mutation", "mention_search.install_registry(registry);"],
  ["input history mutation", "synthetic_input_history", "state.record_prompt(prompt);"],
  ["approval allowlist mutation", "synthetic_allowlist_mutation", "state.approval_allowlist.insert(key);"],
]) {
  expectUnlistedRuntimeMutation(`${family} API is detected`, functionName, body);
}

for (const [label, functionName, body] of [
  [
    "runtime thread direct aliases cannot evade shutdown detection",
    "synthetic_thread_direct_alias",
    "let h = runtime_thread; h.shutdown();",
  ],
  [
    "runtime thread reference aliases cannot evade shutdown detection",
    "synthetic_thread_reference_alias",
    "let h = &runtime_thread; h.shutdown();",
  ],
  [
    "runtime thread clone aliases cannot evade shutdown detection",
    "synthetic_thread_clone_alias",
    "let h = runtime_thread.clone(); h.shutdown();",
  ],
  [
    "runtime thread alias chains cannot evade shutdown detection",
    "synthetic_thread_alias_chain",
    "let first = runtime_thread; let h = &first; h.shutdown();",
  ],
  [
    "runtime host aliases cannot evade shutdown detection",
    "synthetic_host_alias",
    "let h = host.clone(); h.shutdown();",
  ],
  [
    "Goal runtime direct aliases cannot evade pause detection",
    "synthetic_goal_direct_alias",
    "let g = runtime; g.pause(session_id, revision, now);",
  ],
  [
    "Goal runtime reference aliases cannot evade resume detection",
    "synthetic_goal_reference_alias",
    "let g = &runtime; g.resume(session_id, revision, now);",
  ],
  [
    "Goal runtime clone alias chains cannot evade clear detection",
    "synthetic_goal_clone_alias_chain",
    "let first = goal_runtime.clone(); let g = first; g.clear(session_id);",
  ],
  [
    "GoalRuntimeHandle UFCS mutations are detected",
    "synthetic_goal_ufcs",
    "GoalRuntimeHandle::pause(&runtime, session_id, revision, now);",
  ],
  [
    "interaction broker aliases cannot evade response detection",
    "synthetic_broker_alias",
    "let interactions = controller.broker(); interactions.respond(&key, response);",
  ],
  [
    "task registry aliases cannot evade stop detection",
    "synthetic_task_registry_alias",
    "let tasks = task_registry.clone(); tasks.request_stop(task_id);",
  ],
  [
    "task registry aliases cannot evade approval settlement detection",
    "synthetic_approval_registry_alias",
    "let tasks = &task_registry; tasks.finish_denied_pending_tool_approval(task_id);",
  ],
  [
    "approval allowlist aliases cannot evade insert detection",
    "synthetic_allowlist_alias",
    "let allowlist = &mut state.approval_allowlist; allowlist.insert(key);",
  ],
]) {
  expectUnlistedRuntimeMutation(label, functionName, body);
}

for (const [label, functionName, body] of [
  [
    "runtime thread clone receiver chains cannot evade shutdown detection",
    "synthetic_thread_clone_receiver_chain",
    "runtime_thread.clone().shutdown();",
  ],
  [
    "parenthesized runtime thread clone chains cannot evade shutdown detection",
    "synthetic_parenthesized_thread_clone_chain",
    "(runtime_thread.clone()).shutdown();",
  ],
  [
    "associated Arc clone chains cannot evade shutdown detection",
    "synthetic_arc_clone_chain",
    "Arc::clone(&runtime_thread).shutdown();",
  ],
  [
    "parenthesized runtime thread references cannot evade shutdown detection",
    "synthetic_parenthesized_thread_reference",
    "(&runtime_thread).shutdown();",
  ],
  [
    "Goal runtime clone receiver chains cannot evade resume detection",
    "synthetic_goal_clone_receiver_chain",
    "goal_runtime.clone().resume(session_id, revision, now);",
  ],
  [
    "qualified RuntimeThreadHandle UFCS mutations are detected",
    "synthetic_qualified_thread_ufcs",
    "<RuntimeThreadHandle>::shutdown(runtime_thread);",
  ],
  [
    "namespaced qualified RuntimeThreadHandle UFCS mutations are detected",
    "synthetic_namespaced_thread_ufcs",
    "<orca_runtime::RuntimeThreadHandle>::shutdown(runtime_thread);",
  ],
  [
    "qualified GoalRuntimeHandle UFCS mutations are detected",
    "synthetic_qualified_goal_ufcs",
    "<GoalRuntimeHandle>::pause(&runtime, session_id, revision, now);",
  ],
]) {
  expectUnlistedRuntimeMutation(label, functionName, body);
}

for (const [label, functionName, body] of [
  [
    "TuiInteractionBroker self interrupt is mutation authority",
    "synthetic_broker_self_interrupt",
    "self.interrupt(operation_id);",
  ],
  [
    "TuiInteractionBroker self shutdown is mutation authority",
    "synthetic_broker_self_shutdown",
    "self.shutdown();",
  ],
  [
    "TuiInteractionBroker self aliases retain mutation authority",
    "synthetic_broker_self_alias",
    "let broker = self.clone(); broker.shutdown();",
  ],
]) {
  expectFailure(
    label,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(
          `impl TuiInteractionBroker { fn ${functionName}(&self) { ${body} } }`,
        ),
      }),
    new RegExp(`unlisted mutation-capable TUI entrypoint ${functionName}`),
  );
}

{
  const unrelatedMethods = `
fn synthetic_unrelated_same_name_methods() {
    let widget = unrelated_widget;
    widget.shutdown();
    let pager = unrelated_pager;
    pager.pause(session_id, revision, now);
}`;
  expectFailure(
    "unrelated same-name methods require an explicit harmless classification",
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(unrelatedMethods),
      }),
    /unclassified same-name TUI method synthetic_unrelated_same_name_methods/,
  );
}

{
  const cfgTestCalls = `
#[cfg(test)]
fn ignored_cfg_test_helper() {
    runtime_thread.mutate(RuntimeThreadMutation::SetModel(None));
    RuntimeThreadHandle::mutate(runtime_thread, RuntimeThreadMutation::SetModel(None));
    thread.start_turn(request, sink);
    thread.start_turn_with_config(request, sink, config);
    runtime_thread.shutdown();
    RuntimeHost::shutdown(host);
    runtime_thread.launch_workflow(request);
    RuntimeThreadHandle::backtrack_last_user(runtime_thread);
    host.start_thread_with_request(request);
}

#[cfg(test)]
mod ignored_cfg_test_module {
    fn helper() {
        runtime_thread.mutate(RuntimeThreadMutation::SetModel(None));
        thread.start_turn(request, sink);
        runtime_thread.shutdown();
    }
}`;
  assert.doesNotThrow(
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(cfgTestCalls),
      }),
    "cfg(test) functions and modules do not enter the production mutation baseline",
  );
}

{
  const cfgAllTestCalls = `
#[cfg(all(test, unix))]
fn ignored_cfg_all_test_helper() {
    runtime_thread.mutate(RuntimeThreadMutation::SetModel(None));
    operation.interrupt();
    runtime.pause(session_id, revision, now);
}`;
  assert.doesNotThrow(
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(cfgAllTestCalls),
      }),
    "cfg(all(test, unix)) functions do not enter the production mutation baseline",
  );
}

for (const [predicate, functionName] of [
  ["any(test, unix)", "synthetic_cfg_any_test"],
  ["not(test)", "synthetic_cfg_not_test"],
]) {
  expectFailure(
    `cfg(${predicate}) remains in the production mutation scan`,
    () =>
      validateCurrentInventories(cloneManifest(), {
        repoRoot,
        sourceOverrides: appSourceOverride(
          `#[cfg(${predicate})] fn ${functionName}() { runtime_thread.mutate(mutation); }`,
        ),
      }),
    new RegExp(`unlisted mutation-capable TUI entrypoint ${functionName}`),
  );
}

{
  const sources = new Map([
    [
      "crates/orca-tui/src/synthetic/file_module/foo.rs",
      "#[cfg(test)] mod tests;",
    ],
    [
      "crates/orca-tui/src/synthetic/file_module/foo/tests.rs",
      "fn ignored_foo_file_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/directory_module/foo/mod.rs",
      "#[cfg(test)] mod tests;",
    ],
    [
      "crates/orca-tui/src/synthetic/directory_module/foo/tests.rs",
      "fn ignored_foo_mod_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/mod_rs_candidate/bar.rs",
      "#[cfg(all(test, unix))] mod tests;",
    ],
    [
      "crates/orca-tui/src/synthetic/mod_rs_candidate/bar/tests/mod.rs",
      "fn ignored_bar_tests_mod() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/crate_root/lib.rs",
      "#[cfg(test)] mod tests;",
    ],
    [
      "crates/orca-tui/src/synthetic/crate_root/tests.rs",
      "fn ignored_crate_root_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/inline/lib.rs",
      "mod outer { mod inner { #[cfg(test)] mod tests; } }",
    ],
    [
      "crates/orca-tui/src/synthetic/inline/outer/inner/tests.rs",
      "fn ignored_nested_inline_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/explicit/lib.rs",
      'mod outer { #[cfg(test)] #[path = "fixtures/custom.rs"] mod tests; }',
    ],
    [
      "crates/orca-tui/src/synthetic/explicit/outer/fixtures/custom.rs",
      "fn ignored_explicit_path_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/explicit_non_mod/foo.rs",
      'mod outer { #[cfg(test)] #[path = "fixtures/custom.rs"] mod tests; }',
    ],
    [
      "crates/orca-tui/src/synthetic/explicit_non_mod/foo/outer/fixtures/custom.rs",
      "fn ignored_explicit_non_mod_path_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/explicit_parent/lib.rs",
      '#[path = "thread_files"] mod outer { #[cfg(test)] #[path = "custom.rs"] mod tests; }',
    ],
    [
      "crates/orca-tui/src/synthetic/explicit_parent/thread_files/custom.rs",
      "fn ignored_explicit_parent_path_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/sibling/foo.rs",
      "#[cfg(test)] mod tests;",
    ],
    [
      "crates/orca-tui/src/synthetic/sibling/foo/tests.rs",
      "fn ignored_nested_sibling_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/sibling/tests.rs",
      "fn synthetic_sibling_production_tests() { runtime_thread.mutate(mutation); }",
    ],
    [
      "crates/orca-tui/src/synthetic/predicate/lib.rs",
      "#[cfg(any(test, unix))] mod tests;",
    ],
    [
      "crates/orca-tui/src/synthetic/predicate/tests.rs",
      "fn synthetic_cfg_any_external_module() { runtime_thread.mutate(mutation); }",
    ],
  ]);
  const syntheticSites = [
    ...validator.scanTuiMutationEntrypoints({
      repoRoot,
      sourcePaths: [...sources.keys()],
      sourceOverrides: sources,
    }),
  ]
    .filter(([site]) => site.includes("/synthetic/"))
    .sort(([left], [right]) => left.localeCompare(right));
  assert.deepEqual(
    syntheticSites,
    [
      [
        "crates/orca-tui/src/synthetic/predicate/tests.rs:synthetic_cfg_any_external_module:thread.mutate",
        1,
      ],
      [
        "crates/orca-tui/src/synthetic/sibling/tests.rs:synthetic_sibling_production_tests:thread.mutate",
        1,
      ],
    ],
    "external cfg(test) module paths follow Rust file, directory, inline, explicit-path, and predicate rules",
  );
}

{
  const appPath = path.join(repoRoot, "crates/orca-tui/src/app.rs");
  const syntheticApp = `${readFileSync(appPath, "utf8")}\n\
// ignored_comment.mutate(RuntimeThreadMutation::SetModel(None));\n\
const IGNORED_LITERAL: &str = ".mutate(";\n`;
  assert.deepEqual(
    [...validator.scanTuiMutationEntrypoints({
      repoRoot,
      sourceOverrides: new Map([["crates/orca-tui/src/app.rs", syntheticApp]]),
    })],
    productionMutationSites,
    "comment and string contents do not create mutation-capable TUI entrypoints",
  );
}

{
  const relativePath = "crates/orca-tui/src/synthetic/scanner.rs";
  const source = String.raw`
trait BodylessDeclarations {
    async fn wait_for<'a>(&'a self, callback: fn(char)) -> Result<(), Error>;
    fn array(&self, bytes: [u8; 32]);
}

#[cfg(all(test, target_os = "windows"))]
#[allow(clippy::let_unit_value)]
fn ignored_test_only<'a>(value: &'a str) {
    let _character = '\u{7b}';
    runtime_thread.mutate(mutation);
}

#[inline(always)]
fn discovered_mixed_syntax<'a>(value: &'a str) {
    let _raw = r###"fn fake() { runtime_thread.mutate(mutation); }"###;
    let _character = '\x7b';
    nested_macro!({ inner_macro!({ value.len() }) });
    /* fn commented() { runtime_thread.mutate(mutation); } */
    runtime_thread.mutate(mutation);
}
`;
  assert.deepEqual(
    [...validator.scanTuiMutationEntrypoints({
      repoRoot,
      sourcePaths: [relativePath],
      sourceOverrides: new Map([[relativePath, source]]),
    })],
    [[`${relativePath}:discovered_mixed_syntax:thread.mutate`, 1]],
    "Rust scanning handles attributes, bodyless declarations, raw strings, nested macro tokens, comments, lifetimes, character literals, and cfg(test)",
  );
}

for (const [relativePath, minimumOccurrences] of [
  [".github/workflows/release.yml", 1],
  [".github/workflows/windows-ci.yml", 2],
]) {
  const workflow = readFileSync(path.join(repoRoot, relativePath), "utf8");
  const fullHistoryCheckouts = workflow.match(
    /- uses: actions\/checkout@v5\r?\n\s+with:\r?\n\s+fetch-depth: 0/g,
  );
  assert.ok(
    (fullHistoryCheckouts?.length ?? 0) >= minimumOccurrences,
    `${relativePath} must fetch full history for every runtime contract job`,
  );
  for (const command of [
    "node scripts/test-validate-runtime-surface-contract.mjs",
    "node scripts/validate-runtime-surface-contract.mjs",
  ]) {
    assert.ok(
      workflow.split(command).length - 1 >= minimumOccurrences,
      `${relativePath} must run ${command} in every required job`,
    );
  }
}

{
  const attributes = readFileSync(path.join(repoRoot, ".gitattributes"), "utf8");
  for (const rule of [
    "docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-* text eol=lf",
    "docs/superpowers/plans/2026-07-21-runtime-owned-typed-surface-implementation.md text eol=lf",
  ]) {
    assert.ok(
      attributes.split(/\r?\n/).includes(rule),
      `reviewed artifact bytes require ${rule}`,
    );
  }
}

{
  const workflow = readFileSync(
    path.join(repoRoot, ".github/workflows/runtime-contract.yml"),
    "utf8",
  );
  assert.match(
    workflow,
    /- uses: actions\/checkout@v5\r?\n\s+with:\r?\n\s+fetch-depth: 0/,
    "runtime contract workflow must fetch the reviewed design commit",
  );
  for (const command of [
    "node scripts/test-validate-runtime-surface-contract.mjs",
    "node scripts/validate-runtime-surface-contract.mjs",
    "cargo test -p orca-tui runtime_surface_contract --lib --locked",
  ]) {
    assert.ok(workflow.includes(command), `runtime contract workflow must run ${command}`);
  }
}

assert.equal(typeof validator.parseRustEnum, "function", "the Rust enum parser is testable");
assert.deepEqual(
  validator.parseRustEnum(`
pub enum Fixture {
    #[serde(rename = "tuple,renamed")]
    Tuple(String, Vec<u8>), // line comment, with comma
    /* block comment containing FakeVariant, */
    /// doc comment, with comma
    Struct { value: Option<(u8, u8)> },
    #[cfg_attr(feature = "nested", serde(rename = "right]bracket"))]
    #[serde(rename = "right]bracket")]
    RightBracket,
    Final
}`, "pub enum Fixture {"),
  ["Tuple", "Struct", "RightBracket", "Final"],
  "Rust enum parsing handles attributes, comments, payloads, and a final variant without a comma",
);

assert.deepEqual(
  validator.parseRustEnum(String.raw`
pub enum LifetimeFixture<'a> {
    Existing,
    #[doc = "borrowed, static"]
    Hidden(&'static str),
    Named { value: &'a str },
    Character(char),
    Plain = 'x' as isize,
    Newline = '\n' as isize,
    Backslash = '\\' as isize,
    Quote = '\'' as isize,
    Unicode = '界' as isize,
}`, "pub enum LifetimeFixture<'a> {"),
  [
    "Existing",
    "Hidden",
    "Named",
    "Character",
    "Plain",
    "Newline",
    "Backslash",
    "Quote",
    "Unicode",
  ],
  "Rust enum parsing distinguishes lifetimes from character literals",
);

validateRuntimeSurfaceContract({ repoRoot, manifestPath, emitSuccess: false });
console.log("runtime surface contract validator self-tests passed");
