#!/usr/bin/env node

import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const DEFAULT_MANIFEST =
  "docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json";
const DEFAULT_DIGEST =
  "docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json";
const REVIEWED_ARTIFACT_PATHS = [
  "docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md",
  DEFAULT_MANIFEST,
  "docs/superpowers/plans/2026-07-21-runtime-owned-typed-surface-implementation.md",
];
const RUNTIME_SURFACE_MODULES = [
  "commands",
  "commit",
  "host",
  "hub",
  "identity",
  "ingress",
  "interaction",
  "operation",
  "projection",
  "reducer",
  "store",
];
const UNSTABLE_SURFACE_SOURCE_ROOTS = [
  "crates/orca-runtime/src",
  "crates/orca-runtime/tests",
  "crates/orca-tui/src",
];

// Inventory validation revisits the same Rust sources for each closed-world
// mutation fixture. Keep the lexical mask reusable while preserving distinct
// entries for synthetic source overrides used by the self-tests.
const rustNonCodeMaskCache = new Map();

const TABLES = {
  source_fact_columns: "source_facts",
  non_event_source_columns: "non_event_sources",
  thread_command_columns: "thread_commands",
  host_command_columns: "host_commands",
  tui_action_columns: "tui_actions",
  tui_entrypoint_columns: "tui_entrypoints",
  acp_projection_columns: "acp_projection_matrix",
  acp_terminal_mapping_columns: "acp_terminal_mapping",
  acp_pre_reservation_prompt_failure_columns: "acp_pre_reservation_prompt_failure_matrix",
  jsonl_request_columns: "jsonl_request_inventory",
  jsonl_event_columns: "jsonl_event_inventory",
  budget_source_columns: "budget_sources",
  bootstrap_credential_command_columns: "bootstrap_credential_commands",
  cancel_operation_source_state_columns: "cancel_operation_source_state_matrix",
  deferred_state_repair_columns: "deferred_state_repair_matrix",
  acp_family_disposition_columns: "acp_family_dispositions",
  acp_capability_call_columns: "acp_capability_call_inventory",
  acp_prompt_binding_transition_columns: "acp_prompt_binding_transitions",
  acp_capability_settlement_columns: "acp_capability_settlement_matrix",
  operation_transition_columns: "operation_transitions",
  generation_transition_columns: "generation_transitions",
  operation_terminal_mapping_columns: "operation_terminal_mapping",
  history_status_columns: "history_statuses",
  history_item_columns: "history_items",
  interaction_unavailable_settlement_source_columns:
    "interaction_unavailable_settlement_source_matrix",
  post_materialization_recovery_columns: "post_materialization_recovery_matrix",
  goal_continuation_stop_mapping_columns: "goal_continuation_stop_mapping",
  task_status_transition_columns: "task_status_transitions",
  workflow_run_status_transition_columns: "workflow_run_status_transitions",
  workflow_phase_status_transition_columns: "workflow_phase_status_transitions",
  workflow_agent_attempt_transition_columns: "workflow_agent_attempt_transitions",
  subagent_status_transition_columns: "subagent_status_transitions",
  jsonl_supervisor_transition_columns: "jsonl_supervisor_transitions",
  jsonl_supervisor_close_matrix_columns: "jsonl_supervisor_close_matrix",
  jsonl_routing_matrix_columns: "jsonl_routing_matrix",
  jsonl_permission_publication_transition_columns:
    "jsonl_permission_publication_transitions",
  jsonl_opaque_permission_entry_transition_columns:
    "jsonl_opaque_permission_entry_transitions",
  jsonl_direct_interaction_entry_transition_columns:
    "jsonl_direct_interaction_entry_transitions",
};

const ID_TABLES = new Set([
  "source_facts",
  "non_event_sources",
  "thread_commands",
  "host_commands",
  "tui_actions",
  "tui_entrypoints",
  "acp_projection_matrix",
  "acp_terminal_mapping",
  "acp_pre_reservation_prompt_failure_matrix",
  "jsonl_request_inventory",
  "jsonl_event_inventory",
  "budget_sources",
  "bootstrap_credential_commands",
  "cancel_operation_source_state_matrix",
  "deferred_state_repair_matrix",
  "acp_family_dispositions",
  "acp_capability_call_inventory",
  "acp_capability_settlement_matrix",
  "history_statuses",
  "history_items",
  "interaction_unavailable_settlement_source_matrix",
  "goal_continuation_stop_mapping",
  "jsonl_supervisor_transitions",
  "jsonl_supervisor_close_matrix",
  "jsonl_routing_matrix",
  "jsonl_permission_publication_transitions",
]);

const TERMINAL_TRANSITION_STATES = {
  task_status_transitions: ["Stopped", "Completed", "Failed", "Cancelled"],
  workflow_run_status_transitions: ["Stopped", "Completed", "Failed", "Cancelled"],
  workflow_phase_status_transitions: ["Stopped", "Completed", "Failed", "Cancelled"],
  workflow_agent_attempt_transitions: ["Cached", "Completed", "Failed", "Cancelled"],
  subagent_status_transitions: ["Completed", "Failed", "Cancelled"],
};

const COMMAND_CAPABILITIES = new Set([
  "ControlAnyVisibleOperation",
  "ControlBoundOperation",
  "LegacyCancelCurrent",
  "LegacyInterruptResume",
  "LegacyJsonlControl",
  "ManageFolderTrust",
  "ManageGoal",
  "ManageHostSettings",
  "ManageMemory",
  "ManageMemoryWhenMemoryBacked",
  "ManagePinnedContext",
  "ManagePinnedContextWhenPinToThread",
  "ManagePinnedContextWhenTokenHasPin",
  "ManageSessionCatalog",
  "ManageSessionLifecycle",
  "ManageTask",
  "ManageWorkflow",
  "MatchingHostDomainCapability",
  "ReadCatalog",
  "ReadHostPolicy",
  "ReadHostSettings",
  "ReadSessionCatalog",
  "RepairThread",
  "RespondGrantedInteraction",
  "ShutdownHost",
  "SubmitOperation",
]);

const COMMAND_ERRORS = new Set([
  "AdmissionClosed",
  "CapabilityDenied",
  "CapacityExceeded",
  "HostShuttingDown",
  "IllegalState",
  "InvalidContent",
  "InvalidCursor",
  "InvalidInput",
  "InvalidRequest",
  "NotFound",
  "OperationActive",
  "OperationAlreadyTerminal",
  "OperationNotInterrupted",
  "OperationNotSteerable",
  "RuntimeUnavailable",
  "StaleFence",
  "StaleLease",
  "StaleResponseRoute",
  "StaleRevision",
  "StoreUnavailable",
  "ThreadClosed",
  "ThreadOwnedElsewhere",
  "UnknownGeneration",
  "UnknownGoal",
  "UnknownInteraction",
  "UnknownOperation",
  "UnknownSession",
  "UnknownTask",
  "UnknownWorkflow",
  "UnsupportedContent",
  "WrongAttachment",
  "WrongAuthorityFingerprint",
  "WrongHost",
  "WrongInteractionKind",
  "WrongOwnerEpoch",
  "WrongResponseToken",
  "WrongThread",
]);

const COMMAND_FENCES = new Set([
  "CatalogRevision",
  "DurableRevision",
  "GoalCatalogRevision",
  "HostIncarnation",
  "InteractionRevision",
  "MemoryRevision",
  "OptionalSurfaceOperationFenceAfterResolution",
  "PinnedContextRevision",
  "PolicyEpoch",
  "ReconcileMutationToken",
  "RepairThread",
  "ResponseRouteEpoch",
  "ResumeSourceWitness",
  "RetryFinalizationToken",
  "RetryProjectionToken",
  "RetryStartCommitToken",
  "Revision",
  "SessionMetadataRevision",
  "SessionReadToken",
  "SettingsRevision",
  "ShutdownScopeWhenShutdownToken",
  "SurfaceAdmissionLeaseId",
  "SurfaceAdmissionLeaseIdOrSurfaceOperationFence",
  "SurfaceBoundCaller",
  "SurfaceConnectionId",
  "SurfaceCursor",
  "SurfaceGenerationId",
  "SurfaceGoalFence",
  "SurfaceHostBoundCaller",
  "SurfaceOperationFence",
  "SurfaceOperationId",
  "SurfaceResponseGrantToken",
  "SurfaceResponseToken",
  "SurfaceTaskFence",
  "SurfaceWorkflowFence",
  "ThreadOwnerEpoch",
  "TrustRevision",
]);

const ACP_PROJECTION_DISPOSITIONS = new Set([
  "ExtensionOnly",
  "NoWireRetained",
  "PromptTerminal",
  "StandardExact",
  "StandardPlusExtensionMeta",
]);

const ACP_TERMINAL_DISPOSITIONS = new Set([
    "OrcaNotAdmitted",
  "OrcaBudgetExhausted",
  "OrcaOperationFailed",
  "OrcaOperationJoinFailed",
  "OrcaOperationPanicked",
  "OrcaRuntimeRestarted",
  "OrcaTerminalDegraded",
  "PromptResponse::Cancelled",
  "PromptResponse::EndTurn",
  "PromptResponse::MaxTokens",
  "PromptResponse::MaxTurnRequests",
]);

const ACP_PROMPT_FAILURE_DISPOSITIONS = new Set([
  "OrcaBusy",
  "OrcaCapacityExceeded",
  "OrcaInvalidInput",
]);

const TRANSITION_STATE_SETS = {
  operation_transitions: new Set([
    "Requested",
    "Admitted",
    "Admitted(GenerationStarted)",
    "Suspended",
    "Suspended(SuspensionRebasedAfterUnstartedResume)",
    "Finalizing",
    "Finalizing(SuspendedFinalizationCause)",
    "FinalizingDegraded",
    "Terminal",
    "Terminal(NotAdmitted)",
    "Terminal(RetryFinalization)",
    "Terminal(RetryProjectionTerminal)",
  ]),
  generation_transitions: new Set([
    "Reserved",
    "Started",
    "Stopped",
    "Stopped(NotStarted)",
    "Transferred",
  ]),
  task_status_transitions: new Set([
    "Absent",
    "Queued",
    "Running",
    "Paused",
    "Stopping",
    "Stopped",
    "Completed",
    "Failed",
    "ApprovalRequired",
    "Cancelled",
  ]),
  workflow_run_status_transitions: new Set([
    "Absent",
    "Queued",
    "Running",
    "Paused",
    "Stopping",
    "Stopped",
    "Completed",
    "Failed",
    "Cancelled",
    "AsyncLaunched",
  ]),
  workflow_phase_status_transitions: new Set([
    "Absent",
    "Running",
    "Stopped",
    "Completed",
    "Failed",
    "Cancelled",
  ]),
  workflow_agent_attempt_transitions: new Set([
    "Absent",
    "Pending",
    "Running",
    "Cached",
    "Completed",
    "Failed",
    "Cancelled",
  ]),
  subagent_status_transitions: new Set([
    "Absent",
    "Running",
    "Running(Progress)",
    "Completed",
    "Failed",
    "Cancelled",
  ]),
  acp_prompt_binding_transitions: new Set([
    "Decoded",
    "Reserved",
    "Bound",
    "TerminalGated",
    "ResponseWriting",
    "Completed",
    "TransportRetired",
  ]),
  jsonl_supervisor_transitions: new Set([
    "Open",
    "IngressClosed",
    "RoutesRetired",
    "ServicesSettled",
    "RuntimeShutdownPending",
    "Closed",
  ]),
  jsonl_permission_publication_transitions: new Set(["Registered", "Writing", "Published"]),
  jsonl_opaque_permission_entry_transitions: new Set([
    "Routed(Registered|Writing|Published)",
    "Routed(Writing|Published)",
    "CommittedPending",
    "Tombstoned(PermissionCommitted)",
    "Tombstoned(TransportRetired)",
  ]),
  jsonl_direct_interaction_entry_transitions: new Set([
    "Routed(Registered|Writing|Published)",
    "Routed(Writing|Published)",
    "CommittedPending",
    "Tombstoned(DirectInteractionCommitted)",
    "Tombstoned(TransportRetired)",
  ]),
};

const TUI_ENTRYPOINT_ANCHORS = new Map([
  ["slash.new", /SlashCommand::New|UserAction::NewSession/],
  ["slash.model_write", /SlashCommand::Model\(Some|KeyCode::(?:Tab|Enter)/],
  ["slash.model_read", /SlashCommand::Model\(None/],
  [
    "slash.mode_plan_and_backtab",
    /SlashCommand::Mode|SlashCommand::Plan|KeyCode::BackTab|cycle_approval_mode|title == "\/mode"/,
  ],
  ["slash.config_show", /SlashCommand::ConfigShow/],
  ["slash.cost", /SlashCommand::Cost/],
  ["slash.goal", /SlashCommand::Goal/],
  ["slash.workflow_run", /saved workflow|SlashCommand::WorkflowRun/],
  ["slash.workflow_and_agent_panels", /SlashCommand::WorkflowList|SlashCommand::AgentDashboard/],
  ["slash.skills_list", /SlashCommand::SkillList/],
  ["slash.dynamic_skill", /skill alias|SlashCommand::SkillRun/],
  ["slash.remember", /SlashCommand::Remember/],
  ["slash.compact", /SlashCommand::Compact/],
  ["slash.resume", /SlashCommand::Resume|"\/resume"/],
  ["slash.trust_show", /TrustSlashCommand::Show/],
  ["slash.trust_mutation", /TrustSlashCommand::Add|TrustSlashCommand::Remove/],
  ["slash_menu.discovery", /update_slash_menu|available_commands|discover_saved_workflows/],
  ["dispatcher.route_action", /fn route_action/],
  ["approval_always", /fn resolve_approval_option|pending approval|TuiEvent::ApprovalNeeded/],
  [
    "background_approval_reconstruction",
    /open_selected_background_approval_dialog|fn resolve_approval/,
  ],
  [
    "workflow_result_autosubmit",
    /WorkflowResultAvailable|submit_pending_workflow_notification|workflow_notification_turn_boundary/,
  ],
  [
    "background_task_callbacks",
    /submit_background_approval_response_for_tui|stop_task_for_tui|foreground_task_for_tui/,
  ],
  ["recovered_background_scan", /notify_recovered_background_approvals_for_tui/],
  ["startup_session_mcp", /needs_setup|startup_preloaded_transcript|initialize_registry/],
  ["session_picker_transition", /fn handle_session_picker_key|Session picker/],
  [
    "goal_callbacks",
    /fn update_goal_status_for_session|fn show_hosted_goal|fn resume_latest_active_goal_hosted|\bhandle_hosted_goal_action\s*\(/,
  ],
  [
    "mention_catalog_expansion",
    /SearchSessionOptions::new|fn refresh_catalog_async|KeyCode::(?:Tab|Enter)|fn prompt_for_model/,
  ],
  ["setup_api_key", /KeyCode::Enter/],
  ["app_state_update", /fn update\(&mut self, event: TuiEvent\)/],
  ["input_history", /fn load_input_history|fn append_input_history|fn record_prompt/],
  ["terminal_clipboard_notifications", /MouseEventKind::Up|pending_clipboard_copy|desktop_notifications/],
]);

const TUI_ENTRYPOINT_SOURCE_ANCHORS = new Map([
  [
    "session_picker_transition",
    new Map([
      [
        "crates/orca-tui/src/hosted_session_lifecycle.rs",
        /pub\(crate\)\s+fn\s+handle_hosted_session_action\s*\(/,
      ],
      ["crates/orca-tui/src/app.rs", /\bhandle_hosted_session_action\s*\(/],
    ]),
  ],
  [
    "goal_callbacks",
    new Map([
      [
        "crates/orca-tui/src/hosted_session_lifecycle.rs",
        /pub\(crate\)\s+fn\s+resume_latest_active_goal_hosted\s*\(/,
      ],
      [
        "crates/orca-tui/src/hosted_goal.rs",
        /pub\(crate\)\s+fn\s+handle_hosted_goal_action\s*\(/,
      ],
      ["crates/orca-tui/src/app.rs", /\bhandle_hosted_goal_action\s*\(/],
    ]),
  ],
]);

const TUI_ACTION_SOURCE_ANCHORS = new Map([
  [
    "StartSideConversation",
    new Map([
      ["crates/orca-tui/src/app.rs", /\bHostedSideAction::Start\s*\{\s*prompt\s*\}/],
      [
        "crates/orca-tui/src/hosted_side.rs",
        /\bHostedSideAction::Start\s*\{\s*prompt\s*\}\s*=>/,
      ],
    ]),
  ],
  [
    "ToggleSideConversation",
    new Map([
      ["crates/orca-tui/src/app.rs", /\bHostedSideAction::Toggle\b/],
      ["crates/orca-tui/src/hosted_side.rs", /\bHostedSideAction::Toggle\s*=>/],
    ]),
  ],
  [
    "CloseSideConversation",
    new Map([
      ["crates/orca-tui/src/app.rs", /\bHostedSideAction::Close\b/],
      ["crates/orca-tui/src/hosted_side.rs", /\bHostedSideAction::Close\s*=>/],
    ]),
  ],
  [
    "Remember",
    new Map([
      [
        "crates/orca-tui/src/app.rs",
        /\bHostedContextAction::Remember\s*\{\s*scope\s*,\s*note\s*\}/,
      ],
      [
        "crates/orca-tui/src/hosted_context.rs",
        /\bHostedContextAction::Remember\s*\{\s*scope\s*,\s*note\s*\}\s*=>/,
      ],
    ]),
  ],
  [
    "Compact",
    new Map([
      ["crates/orca-tui/src/app.rs", /\bHostedContextAction::Compact\b/],
      ["crates/orca-tui/src/hosted_context.rs", /\bHostedContextAction::Compact\s*=>/],
    ]),
  ],
  [
    "Backtrack",
    new Map([
      ["crates/orca-tui/src/app.rs", /\bHostedContextAction::Backtrack\b/],
      ["crates/orca-tui/src/hosted_context.rs", /\bHostedContextAction::Backtrack\s*=>/],
    ]),
  ],
]);

function tuiEntrypointAnchor(entrypoint, relativePath) {
  return (
    TUI_ENTRYPOINT_SOURCE_ANCHORS.get(entrypoint)?.get(relativePath) ??
    TUI_ENTRYPOINT_ANCHORS.get(entrypoint)
  );
}

function tuiActionSourceAnchor(action, relativePath) {
  return TUI_ACTION_SOURCE_ANCHORS.get(action)?.get(relativePath);
}

const TUI_RUNTIME_MUTATION_APIS = new Map([
  [
    "thread.mutate",
    [/\.\s*mutate\s*\(/g],
  ],
  [
    "thread.start_turn",
    [
      /\.\s*start_turn\s*\(/g,
    ],
  ],
  [
    "thread.start_turn_with_config",
    [
      /\.\s*start_turn_with_config\s*\(/g,
    ],
  ],
  [
    "thread.shutdown",
    [],
  ],
  [
    "host.shutdown",
    [],
  ],
  [
    "thread.launch_workflow",
    [
      /\.\s*launch_workflow\s*\(/g,
    ],
  ],
  [
    "thread.backtrack_last_user",
    [
      /\.\s*backtrack_last_user\s*\(/g,
    ],
  ],
  [
    "host.start_thread_with_request",
    [
      /\.\s*start_thread_with_request\s*\(/g,
    ],
  ],
  ["settings.update", [/\bcycle_approval_mode\s*\(/g]],
  ["policy.update", [/(?<!:)\bset_trust\s*\(/g]],
  [
    "memory.update",
    [/(?<!:)\bremember_(?:user|project)\s*\(/g],
  ],
  ["credentials.update", [/\bsave_api_key\s*\(/g]],
  ["user_action.route", []],
  [
    "operation.interrupt",
    [/\.\s*interrupt\s*\(\s*\)/g],
  ],
  [
    "controller.control",
    [/\.\s*(?:interrupt_current|pause_current_goal|request_background_current)\s*\(/g],
  ],
  [
    "interaction_projection.mutate",
    [
      /\.\s*register_interaction\s*\(/g,
      /\b(?:project_pending_interaction|remove_projected_interaction)\s*\(/g,
    ],
  ],
  [
    "goal.mutate",
    [
      /\bupdate_goal_status_for_session\s*\(/g,
    ],
  ],
  ["workflow.continue", [/\bsubmit_pending_workflow_notification\s*\(/g]],
  [
    "task.mutate",
    [/\b(?:stop_task_for_tui|foreground_task_for_tui)\s*\(/g],
  ],
  [
    "background_approval.respond",
    [/\bsubmit_background_approval_response_for_tui\s*\(/g],
  ],
  ["session.resume", [/\bresume_selected_session\s*\(/g]],
  [
    "catalog.mutate",
    [/\.\s*install_registry\s*\(/g, /(?<!:)\binitialize_registry\s*\(/g],
  ],
  ["input_history.record", [/\.\s*record_prompt\s*\(/g]],
]);

const TUI_RUNTIME_RECEIVER_METHODS = new Map([
  [
    "shutdown",
    [
      ["thread", "thread.shutdown"],
      ["host", "host.shutdown"],
      ["interaction_broker", "interaction_broker.mutate"],
      ["operation_controller", "controller.control"],
      ["action_dispatcher", "controller.control"],
      ["agent_runtime", "host.shutdown"],
    ],
  ],
  [
    "create",
    [["goal", "goal.mutate"]],
  ],
  [
    "edit",
    [["goal", "goal.mutate"]],
  ],
  [
    "clear",
    [["goal", "goal.mutate"]],
  ],
  [
    "pause",
    [["goal", "goal.mutate"]],
  ],
  [
    "resume",
    [["goal", "goal.mutate"]],
  ],
  [
    "resume_into",
    [["goal", "goal.mutate"]],
  ],
  [
    "respond",
    [["interaction_broker", "interaction_broker.mutate"]],
  ],
  [
    "interrupt",
    [["interaction_broker", "interaction_broker.mutate"]],
  ],
  [
    "activate",
    [["interaction_broker", "interaction_broker.mutate"]],
  ],
  [
    "complete",
    [["interaction_broker", "interaction_broker.mutate"]],
  ],
  [
    "register",
    [["interaction_broker", "interaction_broker.mutate"]],
  ],
  [
    "stop",
    [["task_registry", "task.mutate"]],
  ],
  [
    "request_stop",
    [["task_registry", "task.mutate"]],
  ],
  [
    "mark_foregrounded",
    [["task_registry", "task.mutate"]],
  ],
  [
    "submit_pending_tool_approval_response_by_request_id",
    [["task_registry", "background_approval.respond"]],
  ],
  [
    "finish_denied_pending_tool_approval",
    [["task_registry", "background_approval.respond"]],
  ],
  [
    "insert",
    [
      ["approval_allowlist", "approval_allowlist.insert"],
      ["interaction_broker", "interaction_broker.mutate"],
      ["interaction_projection", "interaction_projection.mutate"],
    ],
  ],
]);

function tuiRuntimeAssociatedMethods() {
  const methods = new Map(
    [...TUI_RUNTIME_RECEIVER_METHODS].map(([method, classifications]) => [
      method,
      classifications.map((classification) => [...classification]),
    ]),
  );
  const add = (method, family, api) => {
    const classifications = methods.get(method) ?? [];
    classifications.push([family, api]);
    methods.set(method, classifications);
  };
  const addUnqualified = (method, api) => add(method, "*", api);
  add("mutate", "thread", "thread.mutate");
  add("start_turn", "thread", "thread.start_turn");
  add("start_turn_with_config", "thread", "thread.start_turn_with_config");
  add("launch_workflow", "thread", "thread.launch_workflow");
  add("backtrack_last_user", "thread", "thread.backtrack_last_user");
  add("start_thread_with_request", "host", "host.start_thread_with_request");
  add("interrupt", "operation", "operation.interrupt");
  add("interrupt_current", "operation_controller", "controller.control");
  add("pause_current_goal", "operation_controller", "controller.control");
  add("request_background_current", "operation_controller", "controller.control");
  add("register_interaction", "interaction_projection", "interaction_projection.mutate");
  add("install_registry", "mention_search", "catalog.mutate");
  add("record_prompt", "app_state", "input_history.record");
  addUnqualified("cycle_approval_mode", "settings.update");
  addUnqualified("set_trust", "policy.update");
  addUnqualified("remember_user", "memory.update");
  addUnqualified("remember_project", "memory.update");
  addUnqualified("save_api_key", "credentials.update");
  addUnqualified("project_pending_interaction", "interaction_projection.mutate");
  addUnqualified("remove_projected_interaction", "interaction_projection.mutate");
  addUnqualified("update_goal_status_for_session", "goal.mutate");
  addUnqualified("submit_pending_workflow_notification", "workflow.continue");
  addUnqualified("stop_task_for_tui", "task.mutate");
  addUnqualified("foreground_task_for_tui", "task.mutate");
  addUnqualified(
    "submit_background_approval_response_for_tui",
    "background_approval.respond",
  );
  addUnqualified("resume_selected_session", "session.resume");
  addUnqualified("initialize_registry", "catalog.mutate");
  methods.set("send", []);
  return methods;
}

const TUI_RUNTIME_ASSOCIATED_METHODS = tuiRuntimeAssociatedMethods();

const BASELINE_DIRECT_TUI_MUTATION_SITES = new Map([
  ["crates/orca-tui/src/action_dispatcher.rs:route_action:controller.control", 3],
  ["crates/orca-tui/src/action_dispatcher.rs:drop:controller.control", 1],
  ["crates/orca-tui/src/action_dispatcher.rs:run_dispatcher:controller.control", 1],
  ["crates/orca-tui/src/action_dispatcher.rs:route_action:interaction_broker.mutate", 1],
  ["crates/orca-tui/src/agent_runtime.rs:shutdown:host.shutdown", 2],
  ["crates/orca-tui/src/agent_runtime.rs:spawn_with_dispatch_capacities:controller.control", 1],
  ["crates/orca-tui/src/agent_runtime.rs:shutdown:controller.control", 3],
  ["crates/orca-tui/src/agent_runtime.rs:drop:host.shutdown", 1],
  [
    "crates/orca-tui/src/app.rs:hosted_tui_controller_loop:thread.shutdown",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:resume_latest_active_goal_hosted:thread.shutdown",
    2,
  ],
  [
    "crates/orca-tui/src/app.rs:hosted_tui_controller_loop:thread.launch_workflow",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_context.rs:handle_hosted_context_action:thread.backtrack_last_user",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:ensure_hosted_thread:host.start_thread_with_request",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:resume_latest_active_goal_hosted:host.start_thread_with_request",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:start_new_hosted_session:host.start_thread_with_request",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:start_forked_hosted_session:host.start_thread_with_request",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:switch_saved_hosted_session:host.start_thread_with_request",
    1,
  ],
  ["crates/orca-tui/src/hosted_session_lifecycle.rs:reap_hosted_thread:thread.shutdown", 2],
  ["crates/orca-tui/src/app.rs:run_tui_inner:user_action.route", 2],
  ["crates/orca-tui/src/app.rs:run_tui_inner:host.shutdown", 1],
  [
    "crates/orca-tui/src/app.rs:hosted_tui_controller_loop:task.mutate",
    2,
  ],
  [
    "crates/orca-tui/src/app.rs:hosted_tui_controller_loop:background_approval.respond",
    1,
  ],
  ["crates/orca-tui/src/approval_actions.rs:resolve_approval:user_action.route", 2],
  ["crates/orca-tui/src/approval_actions.rs:resolve_approval_option:approval_allowlist.insert", 2],
  ["crates/orca-tui/src/approval_mode_actions.rs:cycle_approval_mode:user_action.route", 1],
  ["crates/orca-tui/src/global_actions.rs:handle_global_shortcut:user_action.route", 4],
  ["crates/orca-tui/src/idle_navigation_actions.rs:handle_idle_navigation_shortcut:user_action.route", 1],
  ["crates/orca-tui/src/idle_submit_actions.rs:handle_idle_submit:user_action.route", 2],
  ["crates/orca-tui/src/idle_submit_actions.rs:handle_idle_submit:input_history.record", 1],
  ["crates/orca-tui/src/key_event_actions.rs:handle_key_event_preflight:settings.update", 1],
  ["crates/orca-tui/src/plan_approval_actions.rs:implement:user_action.route", 1],
  ["crates/orca-tui/src/running_actions.rs:handle_running_shortcut:user_action.route", 2],
  ["crates/orca-tui/src/runtime_event_actions.rs:handle_runtime_event:user_action.route", 1],
  ["crates/orca-tui/src/runtime_event_actions.rs:handle_runtime_event:workflow.continue", 2],
  ["crates/orca-tui/src/session_picker_actions.rs:handle_session_picker_key:user_action.route", 3],
  ["crates/orca-tui/src/session_picker_actions.rs:activate_action:user_action.route", 2],
  ["crates/orca-tui/src/session_picker_actions.rs:dispatch_selected_resume:user_action.route", 1],
  ["crates/orca-tui/src/setup_actions.rs:handle_setup_key:credentials.update", 2],
  ["crates/orca-tui/src/setup_actions.rs:handle_setup_key:user_action.route", 1],
  ["crates/orca-tui/src/slash_command_actions.rs:handle_slash_command:user_action.route", 12],
  ["crates/orca-tui/src/slash_command_actions.rs:handle_slash_command:input_history.record", 1],
  ["crates/orca-tui/src/slash_menu_actions.rs:handle_slash_menu_key:user_action.route", 1],
  ["crates/orca-tui/src/status_key_actions.rs:handle_recovery_prompt_key:user_action.route", 1],
  ["crates/orca-tui/src/surface_actions.rs:backtrack_last_user:thread.backtrack_last_user", 1],
  ["crates/orca-tui/src/surface_actions.rs:remember:memory.update", 2],
  ["crates/orca-tui/src/surface_actions.rs:save_api_key:credentials.update", 2],
  ["crates/orca-tui/src/queued_input.rs:commit_queued_submission_admission:input_history.record", 1],
  ["crates/orca-tui/src/types.rs:update:input_history.record", 1],
  ["crates/orca-tui/src/workflow_notifications.rs:submit_pending_workflow_notification:user_action.route", 1],
  ["crates/orca-tui/src/workflow_panel_actions.rs:handle_workflows_panel_key:user_action.route", 2],
]);

// Direct calls with an approved typed-surface replacement may be added here
// while they are being retired. The current production baseline has none.
const RETIRABLE_DIRECT_TUI_MUTATION_SITE_MAX_COUNTS = new Map([]);

const BASELINE_HARMLESS_SAME_NAME_METHOD_SITES = new Map([
  ["crates/orca-tui/src/attachment_routing.rs:switch_attachment_deferred:routing.deferred_parent_events.clear", 1],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:install_hosted_session:pending_workflow_notifications.clear",
    1,
  ],
  ["crates/orca-tui/src/app.rs:run_tui_inner:mention_search.shutdown", 1],
  ["crates/orca-tui/src/app.rs:run_tui_inner:terminal.clear", 1],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:start_forked_hosted_session:next_config.prompt.clear",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:start_new_hosted_session:next_config.prompt.clear",
    1,
  ],
  [
    "crates/orca-tui/src/hosted_session_lifecycle.rs:switch_saved_hosted_session:next_config.prompt.clear",
    1,
  ],
  ["crates/orca-tui/src/capability_backend.rs:clear:self.inner.clear", 1],
  ["crates/orca-tui/src/commands/mod.rs:builtin_command_names:names.insert", 1],
  ["crates/orca-tui/src/commands/mod.rs:collect_workflow_dir:seen.insert", 1],
  ["crates/orca-tui/src/diff_highlight.rs:accepts:self.hunk_ranges.insert", 1],
  ["crates/orca-tui/src/diff_highlight.rs:cluster_inline_segments:segments.insert", 4],
  ["crates/orca-tui/src/diff_highlight.rs:compute_file_scoped_styles_with:expected.insert", 1],
  ["crates/orca-tui/src/diff_highlight.rs:compute_file_scoped_styles_with:refined.insert", 1],
  ["crates/orca-tui/src/diff_highlight.rs:reset:self.hunk_ranges.clear", 1],
  ["crates/orca-tui/src/edit_highlight_worker.rs:clear_pending:self.pending.clear", 1],
  ["crates/orca-tui/src/edit_highlight_worker.rs:coalesce_jobs_until_shutdown:positions.insert", 2],
  ["crates/orca-tui/src/edit_highlight_worker.rs:submit:self.pending.clear", 1],
  ["crates/orca-tui/src/edit_highlight_worker.rs:submit:self.pending.insert", 1],
  [
    "crates/orca-tui/src/composer_input_actions.rs:recall_previous_history:state.atomic_skill_tokens.clear",
    1,
  ],
  [
    "crates/orca-tui/src/composer_input_actions.rs:recall_next_history:state.atomic_skill_tokens.clear",
    1,
  ],
  [
    "crates/orca-tui/src/idle_submit_actions.rs:handle_idle_submit:state.atomic_skill_tokens.clear",
    4,
  ],
  ["crates/orca-tui/src/idle_submit_actions.rs:handle_idle_submit:state.mention_bindings.clear", 4],
  ["crates/orca-tui/src/idle_submit_actions.rs:handle_idle_submit:state.pending_pastes.clear", 4],
  ["crates/orca-tui/src/input_adapter.rs:adapt_key:modifiers.insert", 1],
  ["crates/orca-tui/src/input_adapter.rs:adapt_key:state.insert", 2],
  ["crates/orca-tui/src/input_adapter.rs:adapt_modifiers:adapted.insert", 1],
  ["crates/orca-tui/src/input_adapter.rs:adapt:key.modifiers.insert", 1],
  ["crates/orca-tui/src/input_runtime.rs:drive_terminal:driver.resume", 1],
  ["crates/orca-tui/src/input_runtime.rs:resume:self.session.resume", 1],
  ["crates/orca-tui/src/mention_search_manager.rs:drop:self.shutdown", 1],
  ["crates/orca-tui/src/mention_search_manager.rs:sync_at_cursor:state.mention.candidates.clear", 2],
  [
    "crates/orca-tui/src/operation_controller.rs:remember_surface_delivery_watermark:self.lock_hosted().surface_delivery_watermarks.insert",
    1,
  ],
  [
    "crates/orca-tui/src/operation_controller.rs:remember_surface_terminal_delivery:self.lock_hosted().surface_terminal_deliveries.insert",
    1,
  ],
  [
    "crates/orca-tui/src/queued_input_actions.rs:enqueue_composer_follow_up:state.atomic_skill_tokens.clear",
    1,
  ],
  ["crates/orca-tui/src/queued_input_actions.rs:enqueue_composer_follow_up:state.mention_bindings.clear", 1],
  ["crates/orca-tui/src/queued_input_actions.rs:enqueue_composer_follow_up:state.pending_pastes.clear", 1],
  [
    "crates/orca-tui/src/queued_input_actions.rs:restore_latest_queued_message:state.atomic_skill_tokens.clear",
    1,
  ],
  [
    "crates/orca-tui/src/queued_input_actions.rs:reset_after_running_slash:state.atomic_skill_tokens.clear",
    1,
  ],
  ["crates/orca-tui/src/queued_input_actions.rs:reset_after_running_slash:state.mention_bindings.clear", 1],
  ["crates/orca-tui/src/queued_input_actions.rs:reset_after_running_slash:state.pending_pastes.clear", 1],
  ["crates/orca-tui/src/session_picker_actions.rs:close_picker:state.session_picker_query.clear", 1],
  ["crates/orca-tui/src/session_picker_actions.rs:close_picker:state.session_picker_sessions.clear", 1],
  ["crates/orca-tui/src/session_picker_actions.rs:open_session_picker:state.session_picker_query.clear", 1],
  ["crates/orca-tui/src/session_picker_actions.rs:load_next_session_page:seen.insert", 1],
  ["crates/orca-tui/src/shortcuts.rs:normalize_key_parts:modifiers.insert", 1],
  ["crates/orca-tui/src/streaming_markdown.rs:finish:self.current_block.clear", 1],
  ["crates/orca-tui/src/streaming_markdown.rs:finish:self.partial_line.clear", 1],
  ["crates/orca-tui/src/surface_client.rs:parse_workflow_args:args.insert", 2],
  ["crates/orca-tui/src/terminal_presentation.rs:write_pending:self.pending_notifications.clear", 1],
  ["crates/orca-tui/src/terminal_presentation.rs:write_reset_title:self.pending_notifications.clear", 1],
  ["crates/orca-tui/src/transcript_search.rs:clear_matches:self.matches.clear", 1],
  ["crates/orca-tui/src/transcript_search.rs:clear_query:self.matches.clear", 1],
  ["crates/orca-tui/src/transcript_search.rs:clear_query:self.query.clear", 1],
  ["crates/orca-tui/src/transcript_search.rs:insert_char:self.query.insert", 1],
  ["crates/orca-tui/src/transcript_search.rs:open_new:self.matches.clear", 1],
  ["crates/orca-tui/src/transcript_search.rs:open_new:self.query.clear", 1],
  ["crates/orca-tui/src/transcript_search.rs:replace_query:self.query.clear", 1],
  ["crates/orca-tui/src/transcript_view.rs:extract_text:current_line.clear", 1],
  ["crates/orca-tui/src/transcript_view.rs:invalidate:self.dirty_indices.insert", 1],
  ["crates/orca-tui/src/transcript_view.rs:prepare_entry:self.spinner_indices.insert", 1],
  ["crates/orca-tui/src/transcript_view.rs:rebuild_cumulative_heights:self.cumulative_heights.clear", 1],
  ["crates/orca-tui/src/transcript_view.rs:reconcile_len:self.dirty_indices.insert", 1],
  ["crates/orca-tui/src/transcript_view.rs:retain:self.dirty_indices.insert", 1],
  ["crates/orca-tui/src/transcript_view.rs:retain:self.spinner_indices.insert", 1],
  ["crates/orca-tui/src/transcript_view.rs:search:search_index.entries.clear", 1],
  ["crates/orca-tui/src/input_history.rs:append_input_history:std::fs::OpenOptions::new().create", 1],
  ["crates/orca-tui/src/edit_highlight.rs:apply_edit_highlight_result:self.edit_highlights.applied.insert", 1],
  ["crates/orca-tui/src/edit_highlight.rs:clear_applied:self.applied.clear", 1],
  ["crates/orca-tui/src/edit_highlight.rs:reconfigure_edit_highlighting:self.applied.clear", 1],
  ["crates/orca-tui/src/types.rs:clear_projection:self.candidates.clear", 1],
  ["crates/orca-tui/src/types.rs:clear_messages:self.messages.clear", 1],
  ["crates/orca-tui/src/types.rs:clear_messages:self.message_revisions.clear", 1],
  ["crates/orca-tui/src/types.rs:clear_messages:self.tool_call_indices.clear", 1],
  ["crates/orca-tui/src/types.rs:clear_messages:self.transcript_render_cache.clear", 1],
  ["crates/orca-tui/src/types.rs:clear:queue.clear", 1],
  ["crates/orca-tui/src/input_history.rs:load_input_history:seen.insert", 2],
  ["crates/orca-tui/src/types.rs:rebuild_tool_call_indices:self.tool_call_indices.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_message_tracking:self.message_revisions.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_message_tracking:self.transcript_render_cache.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.approval_allowlist.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.atomic_skill_tokens.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.mention_bindings.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.pending_pastes.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.pending_workflow_notifications.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.session_picker_query.clear", 1],
  ["crates/orca-tui/src/types.rs:reset_session_projection:self.session_picker_sessions.clear", 1],
  ["crates/orca-tui/src/types.rs:update:self.atomic_skill_tokens.clear", 1],
  ["crates/orca-tui/src/types.rs:update:self.mention_bindings.clear", 1],
  ["crates/orca-tui/src/ui.rs:append_code_block:source_line.insert", 1],
  ["crates/orca-tui/src/ui.rs:append_proposed_plan_lines:line.spans.insert", 1],
  ["crates/orca-tui/src/ui.rs:render_markdown:opts.insert", 1],
  ["crates/orca-tui/src/ui.rs:render_markdown:current_cell.clear", 1],
  ["crates/orca-tui/src/ui.rs:render_markdown:table_rows.clear", 2],
  ["crates/orca-tui/src/ui.rs:render_table_as_records:lines.insert", 1],
  ["crates/orca-tui/src/ui.rs:render_textarea_visual_line:attached_zero_width.clear", 1],
]);

const BASELINE_HARMLESS_ASSOCIATED_FUNCTION_ITEM_SITES = new Map([
  ["crates/orca-tui/src/scrollback.rs:clear_terminal_scrollback:Terminal::clear", 1],
  ["crates/orca-tui/src/presentation.rs:resume_terminal_render:Terminal::clear", 1],
  [
    "crates/orca-tui/src/surface_actions.rs:launch_workflow:crate::surface_client::launch_workflow",
    1,
  ],
  [
    "crates/orca-tui/src/surface_client.rs:stop_task:WorkflowControlAction::stop",
    1,
  ],
]);
const BASELINE_UNRESOLVED_USER_ACTION_SEND_SITES = new Map([]);

const BASELINE_HARMLESS_ASSOCIATED_FUNCTION_SHA256 = new Map([
  [
    "crates/orca-tui/src/scrollback.rs:clear_terminal_scrollback",
    "6a0f700ce189fe0b8356ee5e61df87c9292f488c979bbb520100502d75b8be8a",
  ],
  [
    "crates/orca-tui/src/presentation.rs:resume_terminal_render",
    "8ff17eeb9d82b6b0f014b64e21d1813e4f26880ffeddee8971da8aac661813dc",
  ],
  [
    "crates/orca-tui/src/surface_actions.rs:launch_workflow",
    "580c07fc16f85dd8fcab1fc16c56b3647a6d550a9a5aef0c5889c68041c151dc",
  ],
  [
    "crates/orca-tui/src/surface_client.rs:stop_task",
    "8cf51d0e4f98d7a31791546397e1729bbb1e37c451de6af6dd1088979891bce0",
  ],
]);
const BASELINE_UNRESOLVED_USER_ACTION_SEND_FUNCTION_SHA256 = new Map([]);

function fail(message) {
  throw new Error(message);
}

function requireArray(value, label) {
  if (!Array.isArray(value)) {
    fail(`${label} must be an array`);
  }
  return value;
}

function requireObject(value, label) {
  if (value === null || Array.isArray(value) || typeof value !== "object") {
    fail(`${label} must be an object`);
  }
  return value;
}

function requireNonemptyString(value, label) {
  if (typeof value !== "string" || value.length === 0) {
    fail(`${label} must be a nonempty string`);
  }
  return value;
}

function assertExactArray(actual, expected, label) {
  if (
    actual.length !== expected.length ||
    actual.some((value, index) => value !== expected[index])
  ) {
    if (label.includes("EventType")) fail("source_facts does not match current EventType");
    if (label.includes("UserAction")) {
      const missing = expected.filter((value) => !actual.includes(value));
      const unexpected = actual.filter((value) => !expected.includes(value));
      const details = [
        missing.length > 0 ? `missing: ${missing.join(", ")}` : undefined,
        unexpected.length > 0 ? `unexpected: ${unexpected.join(", ")}` : undefined,
      ].filter(Boolean);
      fail(
        `current_tui_user_actions does not match current UserAction${
          details.length > 0 ? `; ${details.join("; ")}` : ""
        }`,
      );
    }
    fail(`${label} does not match the baseline inventory`);
  }
}

function assertUnique(values, label) {
  const seen = new Set();
  for (const value of values) {
    requireNonemptyString(value, `${label} id`);
    if (seen.has(value)) {
      fail(`${label} contains duplicate id ${value}`);
    }
    seen.add(value);
  }
}

function validateStringList(values, label, { allowEmpty = false } = {}) {
  requireArray(values, label);
  if (!allowEmpty && values.length === 0) fail(`${label} must not be empty`);
  for (const value of values) requireNonemptyString(value, `${label} value`);
  assertUnique(values, label);
  return values;
}

function assertCondition(condition, message) {
  if (!condition) fail(message);
}

function assertContractKeys(value, keys, label) {
  const contract = requireObject(value, label);
  for (const key of keys) {
    if (!(key in contract)) fail(`${label} is missing ${key}`);
  }
}

function assertContractText(value, required, label) {
  const text = JSON.stringify(value);
  for (const needle of required) {
    if (!text.includes(needle)) fail(`${label} is missing ${needle}`);
  }
}

function hasContent(value) {
  if (Array.isArray(value) || typeof value === "string") return value.length > 0;
  return value !== null && typeof value === "object" && Object.keys(value).length > 0;
}

function getPath(root, dottedPath) {
  let current = root;
  for (const segment of dottedPath.split(".")) {
    if (current === null || typeof current !== "object" || !(segment in current)) {
      return undefined;
    }
    current = current[segment];
  }
  return current;
}

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value !== null && typeof value === "object") {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

function tracedManifest(manifest, paths) {
  return new Proxy(manifest, {
    get(target, property, receiver) {
      if (typeof property === "string" && Object.hasOwn(target, property)) paths.add(property);
      return Reflect.get(target, property, receiver);
    },
  });
}

function assertReviewedFragment(candidate, reviewed, pathName, label) {
  if (!Object.hasOwn(reviewed, pathName)) {
    fail(`reviewed manifest is missing bound path ${pathName} for ${label}`);
  }
  if (canonicalJson(candidate[pathName]) !== canonicalJson(reviewed[pathName])) {
    fail(`reviewed manifest ${pathName} drift for ${label}`);
  }
}

export function canonicalSha256(content) {
  return createHash("sha256").update(content).digest("hex");
}

function canonicalSourceSha256(content) {
  return canonicalSha256(content.replace(/\r\n?/g, "\n"));
}

export function parseManifestText(text) {
  try {
    return JSON.parse(text);
  } catch (error) {
    fail(`malformed manifest JSON: ${error.message}`);
  }
}

function validateTables(manifest) {
  const declaredColumnKeys = Object.keys(manifest).filter((key) => key.endsWith("_columns"));
  assertExactArray(declaredColumnKeys.sort(), Object.keys(TABLES).sort(), "declared table schemas");

  for (const [columnsKey, tableKey] of Object.entries(TABLES)) {
    const columns = requireArray(manifest[columnsKey], columnsKey);
    const rows = requireArray(manifest[tableKey], tableKey);
    assertUnique(columns, columnsKey);
    rows.forEach((row, index) => {
      if (!Array.isArray(row)) {
        fail(`${tableKey} row ${index} must be an array`);
      }
      if (row.length !== columns.length) {
        fail(`${tableKey} row ${index} has width ${row.length}; expected ${columns.length}`);
      }
    });
    if (ID_TABLES.has(tableKey)) {
      assertUnique(rows.map((row) => row[0]), tableKey);
    }
    assertUnique(rows.map((row) => JSON.stringify(row)), `${tableKey} rows`);
  }
}

function validateClosedInventories(manifest) {
  const closed = requireObject(manifest.closed_inventory, "closed_inventory");
  for (const [name, values] of Object.entries(closed)) {
    if (Array.isArray(values) && values.every((value) => typeof value === "string")) {
      assertUnique(values, `closed_inventory.${name}`);
    }
  }

  const sourceFamilies = new Set([...closed.surface_event_families, "None"]);
  const sourceTargets = new Set(closed.source_target_variants);
  const sourceScopes = new Set(closed.source_scopes);
  const tuiDispositions = new Set(manifest.adapter_dispositions.tui);
  const acpDispositions = new Set(manifest.adapter_dispositions.acp);
  const jsonlDispositions = new Set(manifest.adapter_dispositions.jsonl);
  for (const row of manifest.source_facts) {
    const [variant, , , family, target, scope, , , , tui, acp, jsonl] = row;
    if (!sourceFamilies.has(family)) fail(`${variant} has unknown target family ${family}`);
    if (!sourceTargets.has(target)) fail(`${variant} has unknown target variant ${target}`);
    if (!sourceScopes.has(scope)) fail(`${variant} has unknown source scope ${scope}`);
    if (!tuiDispositions.has(tui)) fail(`${variant} has unknown TUI disposition ${tui}`);
    if (!acpDispositions.has(acp)) fail(`${variant} has unknown ACP disposition ${acp}`);
    if (!jsonlDispositions.has(jsonl)) fail(`${variant} has unknown JSONL disposition ${jsonl}`);
  }

  const nonEventTargets = new Set([
    ...closed.source_target_variants,
    ...closed.non_event_target_variants,
  ]);
  const materializations = new Set(closed.non_event_materialization);
  for (const row of manifest.non_event_sources) {
    const [id, , , , family, target, materialization] = row;
    if (!sourceFamilies.has(family)) fail(`${id} has unknown target family ${family}`);
    if (!nonEventTargets.has(target)) fail(`${id} has unknown target variant ${target}`);
    if (!materializations.has(materialization)) {
      fail(`${id} has unknown materialization ${materialization}`);
    }
  }
}

function validateRuntimeSurfacePublicExportManifest(manifest) {
  const exportsByModule = requireObject(
    manifest.runtime_surface_public_exports,
    "runtime_surface_public_exports",
  );
  assertExactArray(
    Object.keys(exportsByModule),
    RUNTIME_SURFACE_MODULES,
    "runtime_surface_public_exports modules",
  );
  for (const moduleName of RUNTIME_SURFACE_MODULES) {
    const names = requireArray(
      exportsByModule[moduleName],
      `runtime_surface_public_exports.${moduleName}`,
    );
    names.forEach((name, index) =>
      requireNonemptyString(name, `runtime_surface_public_exports.${moduleName}[${index}]`),
    );
    assertUnique(names, `runtime_surface_public_exports.${moduleName}`);
    assertExactArray(
      names,
      [...names].sort(),
      `runtime_surface_public_exports.${moduleName} sorted order`,
    );
  }
  assertUnique(
    RUNTIME_SURFACE_MODULES.flatMap((moduleName) => exportsByModule[moduleName]),
    "runtime_surface_public_exports flattened names",
  );
}

function validateCommands(manifest, tableKey, inventoryKey, dispositionKey) {
  const closed = manifest.closed_inventory;
  const rows = manifest[tableKey];
  assertExactArray(
    rows.map((row) => row[0]),
    closed[inventoryKey],
    `${tableKey} names`,
  );

  const targets = new Set(closed.command_targets);
  const dispositions = new Set(closed[dispositionKey]);
  const acknowledgements = new Set(closed.acknowledgement_forms);
  const deferredValues = new Set(closed.deferred_command_values);
  const emittedFacts = new Set(closed.emitted_fact_forms);

  for (const row of rows) {
    const name = row[0];
    const target = row[2];
    const capabilities = validateStringList(row[3], `${name} capabilities`);
    const fences = validateStringList(row[4], `${name} fences`, { allowEmpty: true });
    validateStringList(row[5], `${name} legal states or preconditions`);
    const normalDispositions = requireArray(row[7], `${name} normal dispositions`);
    const requiredAcknowledgements = requireArray(row[9], `${name} required acknowledgements`);
    const legalDeferredValues = requireArray(row[10], `${name} legal deferred values`);
    if (!targets.has(target)) fail(`${name} has unknown target ${target}`);
    if (capabilities.length === 0) fail(`${name} has no capability`);
    if (normalDispositions.length === 0) fail(`${name} has no normal disposition`);
    for (const disposition of normalDispositions) {
      if (!dispositions.has(disposition)) {
        fail(`${name} has unknown disposition ${disposition}`);
      }
    }
    for (const capability of capabilities) {
      if (!COMMAND_CAPABILITIES.has(capability)) {
        fail(`${name} has unknown capability ${capability}`);
      }
    }
    for (const fence of fences) {
      if (!COMMAND_FENCES.has(fence)) fail(`${name} has unknown fence ${fence}`);
    }
    for (const acknowledgement of requiredAcknowledgements) {
      if (!acknowledgements.has(acknowledgement)) {
        fail(`${name} has unknown acknowledgement ${acknowledgement}`);
      }
    }
    for (const deferred of legalDeferredValues) {
      if (!deferredValues.has(deferred)) fail(`${name} has unknown deferred value ${deferred}`);
    }
    if (
      normalDispositions.some((value) => value === "Accepted" || value === "AlreadyApplied") &&
      requiredAcknowledgements.length === 0
    ) {
      fail(`${name} has no required acknowledgements`);
    }
    if (tableKey === "thread_commands") {
      for (const emitted of requireArray(row[11], `${name} emitted facts`)) {
        if (!emittedFacts.has(emitted)) fail(`${name} has unknown emitted fact ${emitted}`);
      }
    }
    const errors = validateStringList(row[12], `${name} command errors`);
    for (const error of errors) {
      if (!COMMAND_ERRORS.has(error)) fail(`${name} has unknown command error ${error}`);
    }
  }
}

function validateAcpDispositions(manifest) {
  for (const tableKey of ["acp_projection_matrix", "acp_family_dispositions"]) {
    for (const row of manifest[tableKey]) {
      for (const disposition of row.slice(1, 3)) {
        if (!ACP_PROJECTION_DISPOSITIONS.has(disposition)) {
          fail(`unknown ACP projection disposition ${disposition}`);
        }
      }
    }
  }
  for (const row of manifest.acp_terminal_mapping) {
    if (!ACP_TERMINAL_DISPOSITIONS.has(row[1])) {
      fail(`unknown ACP terminal disposition ${row[1]}`);
    }
  }
  for (const row of manifest.acp_pre_reservation_prompt_failure_matrix) {
    if (!ACP_PROMPT_FAILURE_DISPOSITIONS.has(row[1])) {
      fail(`unknown ACP pre-reservation disposition ${row[1]}`);
    }
  }
}

function validateAcknowledgementForms(manifest) {
  const closedForms = new Set(manifest.closed_inventory.acknowledgement_forms);
  const semantics = requireObject(
    manifest.acknowledgement_form_semantics,
    "acknowledgement_form_semantics",
  );
  for (const inventoryName of [
    "thread_conditional_acknowledgement_forms",
    "host_conditional_acknowledgement_forms",
  ]) {
    const forms = requireArray(manifest[inventoryName], inventoryName);
    assertUnique(forms, inventoryName);
    for (const form of forms) {
      if (!closedForms.has(form)) fail(`${inventoryName} contains unknown form ${form}`);
      if (!Object.hasOwn(semantics, form)) fail(`${form} has no acknowledgement semantics`);
    }
  }
}

function validateTransitions(manifest) {
  for (const [tableKey, allowedStates] of Object.entries(TRANSITION_STATE_SETS)) {
    for (const [source, target] of manifest[tableKey]) {
      if (!allowedStates.has(source)) {
        fail(`${tableKey} has unknown source state ${source}`);
      }
      if (!allowedStates.has(target)) {
        fail(`${tableKey} has unknown target state ${target}`);
      }
    }
  }
  for (const tableKey of ["operation_transitions", "generation_transitions"]) {
    manifest[tableKey].forEach((row, index) => {
      if (typeof row[2] !== "string" || row[2].length === 0) {
        fail(`${tableKey} row ${index} has no trigger`);
      }
      validateStringList(row[3], `${tableKey} row ${index} invariants`);
    });
  }
  for (const [tableKey, terminals] of Object.entries(TERMINAL_TRANSITION_STATES)) {
    const seen = new Set();
    for (const [source, target] of manifest[tableKey]) {
      const edge = `${source}->${target}`;
      if (seen.has(edge)) fail(`${tableKey} contains duplicate transition ${edge}`);
      seen.add(edge);
      const sourceBase = source.split("(")[0];
      if (terminals.includes(sourceBase)) {
        fail(`${tableKey} allows terminal source ${sourceBase} to transition`);
      }
    }
  }

  if (manifest.operation_transitions.length !== 11) {
    fail("operation_transitions must contain exactly 11 legal edges");
  }
  if (manifest.generation_transitions.length !== 5) {
    fail("generation_transitions must contain exactly 5 legal edges");
  }
  if (manifest.operation_terminal_mapping.length !== 65) {
    fail("operation_terminal_mapping must contain exactly 65 rows");
  }
}

function validateGeneratorReference(manifest, generatorId, reference) {
  if (typeof reference !== "string" || getPath(manifest, reference) === undefined) {
    fail(`test generator ${generatorId} references missing source ${reference}`);
  }
}

function validateTestGenerators(manifest) {
  const generators = requireArray(manifest.test_vector_generators, "test_vector_generators");
  assertUnique(generators.map((generator) => generator.id), "test_vector_generators");
  for (const generator of generators) {
    requireObject(generator, `test generator ${generator.id}`);
    const references = [];
    if (generator.source !== undefined) references.push(...[].concat(generator.source));
    if (generator.variants_from !== undefined) references.push(generator.variants_from);
    if (generator.patches_from !== undefined) references.push(generator.patches_from);
    for (const reference of references) {
      validateGeneratorReference(manifest, generator.id, reference);
    }
    if (Object.keys(generator).length < 2) {
      fail(`test generator ${generator.id} has no generator axes`);
    }
  }
}

function commandRow(manifest, table, name) {
  const row = manifest[table].find((candidate) => candidate[0] === name);
  if (!row) fail(`${table} is missing ${name}`);
  return row;
}

function invariantRegistry() {
  return new Map([
    [
      "source_facts has exactly 53 unique variants matching EventType at baseline",
      (manifest) => {
        assertCondition(manifest.source_facts.length === 53, "source_facts must contain 53 rows");
        assertUnique(manifest.source_facts.map((row) => row[0]), "source_facts");
      },
    ],
    [
      "closed_inventory.current_tui_user_actions has exactly 36 unique variants matching UserAction at baseline",
      (manifest) => {
        assertCondition(
          manifest.closed_inventory.current_tui_user_actions.length === 36,
          "current_tui_user_actions must contain 36 variants",
        );
        assertUnique(
          manifest.closed_inventory.current_tui_user_actions,
          "current_tui_user_actions",
        );
      },
    ],
    [
      "thread_commands has exactly 22 unique names matching closed_inventory.surface_commands",
      (manifest) =>
        assertExactArray(
          manifest.thread_commands.map((row) => row[0]),
          manifest.closed_inventory.surface_commands,
          "thread_commands names",
        ),
    ],
    [
      "host_commands has exactly 24 unique names matching closed_inventory.surface_host_commands including ControlJsonlTurn",
      (manifest) => {
        assertExactArray(
          manifest.host_commands.map((row) => row[0]),
          manifest.closed_inventory.surface_host_commands,
          "host_commands names",
        );
        assertCondition(
          manifest.closed_inventory.surface_host_commands.includes("ControlJsonlTurn"),
          "surface_host_commands must include ControlJsonlTurn",
        );
      },
    ],
    [
      "every row length equals its declared column count and every inventory id is unique",
      (manifest) => {
        assertCondition(Object.keys(TABLES).length === 38, "all 38 declared tables must be validated");
        for (const [columns, table] of Object.entries(TABLES)) {
          assertCondition(
            manifest[table].every((row) => row.length === manifest[columns].length),
            `${table} contains a wrong-width row`,
          );
        }
      },
    ],
    [
      "every source fact has a closed source scope, explicit target migration, and closed TUI ACP JSONL disposition",
      (manifest) => {
        const scopes = new Set(manifest.closed_inventory.source_scopes);
        assertCondition(
          manifest.source_facts.every(
            (row) =>
              scopes.has(row[5]) &&
              typeof row[8] === "string" &&
              row[8].length > 0 &&
              manifest.adapter_dispositions.tui.includes(row[9]) &&
              manifest.adapter_dispositions.acp.includes(row[10]) &&
              manifest.adapter_dispositions.jsonl.includes(row[11]),
          ),
          "source_facts contains an open scope, migration, or adapter disposition",
        );
      },
    ],
    [
      "every non-event source has explicit source identity and closed materialization disposition",
      (manifest) => {
        const dispositions = new Set(manifest.closed_inventory.non_event_materialization);
        assertCondition(
          manifest.non_event_sources.every(
            (row) => typeof row[3] === "string" && row[3].length > 0 && dispositions.has(row[6]),
          ),
          "non_event_sources contains an open identity or materialization",
        );
      },
    ],
    [
      "every command has target capability precondition or legal state fences idempotency closed normal dispositions result required acknowledgements legal deferred values effect or emitted facts and closed errors",
      (manifest) => {
        for (const row of [...manifest.thread_commands, ...manifest.host_commands]) {
          assertCondition(
            row[2] && row[3].length && row[4] && row[5].length && row[6] && row[7].length &&
              row[8] && row[10].length && row[11] !== undefined && row[12].length,
            `${row[0]} has an incomplete command contract`,
          );
        }
      },
    ],
    [
      "every command disposition acknowledgement deferred value and target belongs to its closed inventory",
      (manifest) => {
        const closed = manifest.closed_inventory;
        for (const row of [...manifest.thread_commands, ...manifest.host_commands]) {
          assertCondition(closed.command_targets.includes(row[2]), `${row[0]} has an open target`);
          assertCondition(
            row[9].every((value) => closed.acknowledgement_forms.includes(value)),
            `${row[0]} has an open acknowledgement`,
          );
          assertCondition(
            row[10].every((value) => closed.deferred_command_values.includes(value)),
            `${row[0]} has an open deferred value`,
          );
        }
      },
    ],
    [
      "AdmitReserved does not emit a duplicate GenerationReserved fact because Operation.Admitted carries first_generation",
      (manifest) => {
        const emits = commandRow(manifest, "thread_commands", "AdmitReserved")[11];
        assertCondition(emits.includes("Operation.Admitted"), "AdmitReserved must emit Operation.Admitted");
        assertCondition(
          !emits.includes("Operation.GenerationReserved"),
          "AdmitReserved must not emit duplicate GenerationReserved",
        );
      },
    ],
    [
      "RetryFinalization emits only missing settlement or Operation.Terminal and never FinalizationStarted",
      (manifest) => {
        const emits = commandRow(manifest, "thread_commands", "RetryFinalization")[11];
        assertCondition(
          emits.every(
            (value) =>
              value === "Operation.FinalizationSettlementRecorded" || value === "Operation.Terminal",
          ),
          "RetryFinalization has an illegal emitted fact",
        );
        assertCondition(
          !emits.some((value) => value.includes("FinalizationStarted")),
          "RetryFinalization must not emit FinalizationStarted",
        );
      },
    ],
    [
      "CancelOperation terminal retry is AlreadyApplied and CloseThread or ShutdownHost unresolved terminals are Deferred",
      (manifest) => {
        const terminal = manifest.cancel_operation_source_state_matrix.find(
          (row) => row[0] === "Terminal",
        );
        assertCondition(terminal?.[1] === "AlreadyApplied", "terminal cancel must be AlreadyApplied");
        for (const name of ["CloseThread", "ShutdownHost"]) {
          assertCondition(
            commandRow(manifest, "host_commands", name)[10].includes("NoValue/ShutdownDeferred"),
            `${name} must allow ShutdownDeferred`,
          );
        }
      },
    ],
    [
      "ACP projection matrix is total for StandardOnly and OrcaSurfaceV1 and every cell is a closed AcpProjectionDisposition",
      (manifest) => {
        assertExactArray(
          manifest.acp_projection_profiles,
          ["StandardOnly", "OrcaSurfaceV1"],
          "ACP projection profiles",
        );
        assertCondition(manifest.acp_projection_matrix.length === 28, "ACP projection matrix must be total");
        assertCondition(
          manifest.acp_projection_matrix.every((row) =>
            row.slice(1, 3).every((value) => ACP_PROJECTION_DISPOSITIONS.has(value)),
          ),
          "ACP projection matrix contains an open disposition",
        );
      },
    ],
    [
      "JSONL request event ordering permission router multi-loop turn control history and permission override inventories are exact to baseline",
      (manifest) => {
        assertCondition(manifest.jsonl_request_inventory.length === 38, "JSONL requests must remain exact");
        assertCondition(manifest.jsonl_event_inventory.length === 41, "JSONL events must remain exact");
        for (const key of [
          "jsonl_ordering_vectors",
          "jsonl_permission_router_vectors",
          "jsonl_multi_loop_turn_vector",
          "jsonl_legacy_turn_control_vectors",
          "jsonl_history_query_vectors",
          "jsonl_resume_override_vectors",
        ]) {
          assertCondition(hasContent(manifest[key]), `${key} must not be empty`);
        }
      },
    ],
    [
      "bootstrap credential persistence is isolated from runtime surface authority and has one closed command",
      (manifest) => {
        assertCondition(
          manifest.bootstrap_credential_commands.length === 1 &&
            manifest.bootstrap_credential_commands[0][0] === "StoreProviderCredential",
          "bootstrap credential command inventory must remain singular",
        );
        assertContractText(
          manifest.bootstrap_credential_commands[0],
          ["never_surface_event_or_snapshot"],
          "bootstrap credential command",
        );
      },
    ],
    [
      "every mutation-capable TUI entrypoint has target route required fences result consumer and Phase 3 disposition",
      (manifest) =>
        assertCondition(
          manifest.tui_entrypoints.every(
            (row) => row[4] && Array.isArray(row[5]) && row[6] && row[7],
          ),
          "TUI entrypoint has an incomplete migration boundary",
        ),
    ],
    [
      "no authoritative recorded fact lacks coordinator receipt or recorded durability",
      (manifest) =>
        assertCondition(
          manifest.source_facts.every(
            (row) =>
              row[2] !== "recorded" ||
              row[3] === "None" ||
              String(row[6]).includes("recorded"),
          ),
          "recorded source fact lacks recorded target durability",
        ),
    ],
    [
      "no open JSON value wildcard terminal or implementation-decides outcome exists",
      (manifest) => {
        const serialized = JSON.stringify(manifest);
        for (const forbidden of [
          "serde_json::Value",
          "implementation_decides",
          "WildcardTerminal",
        ]) {
          assertCondition(!serialized.includes(forbidden), `manifest contains forbidden ${forbidden}`);
        }
      },
    ],
    [
      "ACP capability call and remote terminal lease inventories match the private settlement matrix",
      (manifest) => {
        assertCondition(manifest.acp_capability_call_inventory.length === 7, "ACP calls must have 7 kinds");
        assertCondition(
          manifest.acp_capability_settlement_matrix.length === 5,
          "ACP capability settlement matrix must have 5 rows",
        );
        assertExactArray(
          manifest.acp_remote_terminal_lease_states,
          ["Live", "KillPending", "ReleasePending", "Released", "IdentityUnknown", "CleanupAmbiguous"],
          "ACP remote terminal lease states",
        );
      },
    ],
    [
      "JSONL permission/respond routes through one opaque router before owner selection",
      (manifest) => {
        const route = manifest.jsonl_routing_matrix.find((row) => row[0] === "permission/respond");
        assertCondition(
          route?.[1] === "OpaquePermissionRouter",
          "permission/respond must be owned by OpaquePermissionRouter",
        );
      },
    ],
    [
      "JSONL permission and direct ledgers enforce one bounded atomic live-admission counter, repair-authority permit, and owner-specific rejection settlement before frame encoding",
      (manifest) => {
        assertContractKeys(
          manifest.jsonl_live_request_contract,
          ["shared_count", "per_connection_limit", "permit_lifecycle", "rejection_settlements"],
          "jsonl_live_request_contract",
        );
        assertCondition(
          manifest.jsonl_permission_publication_transitions.length > 0 &&
            manifest.jsonl_direct_interaction_entry_transitions.length > 0,
          "JSONL permission/direct ledgers must be closed",
        );
      },
    ],
    [
      "JSONL transport retirement tombstones only after an exact owner settlement; DeferredToRuntime creates and transfers its durable repair record first",
      (manifest) => {
        assertContractKeys(
          manifest.jsonl_tombstone_contract,
          ["transport_retirement", "retirement_sequence_allocator", "owner_rank"],
          "jsonl_tombstone_contract",
        );
        assertContractText(
          manifest.jsonl_runtime_durable_repair_contract,
          ["DeferredToRuntime", "transfer"],
          "jsonl_runtime_durable_repair_contract",
        );
      },
    ],
    [
      "JSONL lookup and insertion expire tombstones with one shared sequence allocator and closed owner ranks",
      (manifest) =>
        assertContractKeys(
          manifest.jsonl_tombstone_contract,
          ["cleanup_before", "retirement_sequence_allocator", "owner_rank", "expiry_order"],
          "jsonl_tombstone_contract",
        ),
    ],
    [
      "JSONL close freezes an exact repair plan, transfers each durable repair record to the runtime recovery owner before tombstoning, requires exact repair and fixed four-service coverage, and retains one typed IO failure before the sole ShutdownHost rail",
      (manifest) => {
        assertContractKeys(
          manifest.jsonl_committed_repair_contract,
          ["plan_freeze", "settlement_set", "shutdown_barrier"],
          "jsonl_committed_repair_contract",
        );
        assertContractKeys(
          manifest.jsonl_service_settlement_contract,
          ["coverage", "fixed_fields", "result_precedence"],
          "jsonl_service_settlement_contract",
        );
        assertCondition(
          manifest.jsonl_supervisor_close_matrix.length === 6,
          "JSONL supervisor close matrix must have 6 triggers",
        );
      },
    ],
    [
      "interaction reverse response failure authority is separate from runtime capability-ledger and owning-tool settlement failure authority",
      (manifest) => {
        const boundaries = manifest.authority_boundaries;
        assertCondition(
          boundaries.interaction_reverse_response_failure_authority !==
            boundaries.capability_ledger_failure_authority,
          "interaction and capability failure authority must be separate",
        );
      },
    ],
    [
      "ACP transport never publishes or fabricates OperationTerminal",
      (manifest) =>
        assertCondition(
          manifest.authority_boundaries.acp_transport_terminal_authority ===
            "TransportNeverPublishesOperationTerminal",
          "ACP transport terminal authority has drifted",
        ),
    ],
    [
      "ACP pre-reservation InvalidInput OperationActive and CapacityExceeded are RPC failures and never fabricated NotAdmitted terminals",
      (manifest) =>
        assertExactArray(
          manifest.acp_pre_reservation_prompt_failure_matrix.map((row) => row[0]),
          ["InvalidInput", "OperationActive", "CapacityExceeded"],
          "ACP pre-reservation failures",
        ),
    ],
    [
      "interaction snapshots and events contain no response or grant secrets; response authority is injected by the bound handle",
      (manifest) => {
        assertContractText(
          manifest.interaction_secret_boundary.snapshot_fields_forbidden,
          ["response_secret", "grant_secret", "authority_fingerprint_secret"],
          "interaction secret boundary",
        );
        assertCondition(
          manifest.interaction_secret_boundary.response_authority ===
            "attachment_bound_handle_injected",
          "interaction response authority must be handle injected",
        );
      },
    ],
    [
      "operation and generation transition inventories are closed and every unlisted transition is IllegalTransition",
      (manifest) => {
        assertCondition(manifest.operation_transitions.length === 11, "operation transitions must be closed");
        assertCondition(manifest.generation_transitions.length === 5, "generation transitions must be closed");
        assertContractText(
          [manifest.operation_transition_columns, manifest.generation_transition_columns],
          ["source_phase", "target_phase", "trigger_or_patch"],
          "transition schemas",
        );
      },
    ],
    [
      "operation-generation cross-field invariants are mandatory reducer checks",
      (manifest) =>
        assertCondition(
          manifest.operation_generation_invariants.length === 16 &&
            manifest.operation_generation_invariants.every((value) => value.length > 0),
          "operation_generation_invariants must contain 16 mandatory checks",
        ),
    ],
    [
      "history status role item metadata orphan tool result and workflow shapes match the released baseline",
      (manifest) => {
        assertCondition(manifest.history_statuses.length === 9, "history statuses must have 9 rows");
        assertCondition(manifest.history_items.length === 12, "history items must have 12 rows");
        assertCondition(
          manifest.history_contract_invariants.length === 7,
          "history contract invariants must have 7 rows",
        );
        assertCondition(Object.keys(manifest.history_message_role_map).length > 0, "history role map is empty");
      },
    ],
    [
      "FreshAttachRequest and CursorAttachRequest have disjoint success DTOs and cursor attach carries no snapshot",
      (manifest) => {
        assertExactArray(
          manifest.attach_contract.request_variants,
          ["FreshAttachRequest", "CursorAttachRequest"],
          "attach request variants",
        );
        assertCondition(
          !JSON.stringify(manifest.attach_contract.cursor_attachment_fields).includes("snapshot"),
          "cursor attachment must not carry a snapshot",
        );
      },
    ],
    [
      "Requested CancelOperation commits NotAdmitted CancelledBeforeAdmission synchronously with cursor then terminal witness and no control intent",
      (manifest) => {
        const requested = manifest.cancel_operation_source_state_matrix.find(
          (row) => row[0] === "Requested",
        );
        assertExactArray(
          requested[2],
          ["ThreadCursor(Operation)", "OperationTerminal(operation_id)"],
          "Requested CancelOperation acknowledgements",
        );
        assertContractText(requested[4], ["CancelledBeforeAdmission"], "Requested cancel facts");
        assertCondition(
          !JSON.stringify(requested[4]).includes("ControlIntentCommitted"),
          "Requested cancel must not emit control intent",
        );
      },
    ],
    [
      "Goal generation identity is exact across Goal admission generation records replay and predecessor authorization",
      (manifest) => {
        assertContractKeys(
          manifest.goal_generation_identity_contract,
          ["fields", "attempts", "invariants", "outer_turn_origins"],
          "goal_generation_identity_contract",
        );
        assertCondition(
          manifest.goal_generation_identity_contract.invariants.length > 0,
          "goal generation identity invariants are empty",
        );
      },
    ],
    [
      "all revision boundaries use non-interchangeable typed wrappers",
      (manifest) => {
        assertUnique(manifest.revision_domain_variants, "revision_domain_variants");
        assertCondition(
          manifest.revision_domain_invariants.length === 5,
          "revision domain invariants must have 5 checks",
        );
        assertContractText(
          manifest.revision_domain_invariants,
          ["never compared across domains", "generic Revision is forbidden"],
          "revision domain invariants",
        );
      },
    ],
    [
      "McpCatalogQuery Lookup miss is outer SurfaceReadResult NotFound and Entry is non-optional",
      (manifest) => {
        assertCondition(
          manifest.mcp_catalog_query_contract.lookup_miss === "SurfaceReadResult.NotFound",
          "MCP lookup miss must be outer NotFound",
        );
        assertCondition(
          manifest.mcp_catalog_query_contract.page_value_variants.includes(
            "Entry(SurfaceCatalogEntry)",
          ),
          "MCP catalog Entry must be non-optional",
        );
      },
    ],
    [
      "JSONL runtimeWorkspaceRoots presence distinguishes omitted from explicit empty clear",
      (manifest) =>
        assertContractText(
          manifest.runtime_settings_patch_contract.jsonl_presence_rules,
          ["runtimeWorkspaceRoots", "omitted", "empty"],
          "JSONL runtime settings presence rules",
        ),
    ],
    [
      "SurfaceDataValue preserves unsigned integers and sorted unique DisplayText object keys including empty",
      (manifest) => {
        assertCondition(
          manifest.surface_data_value_variants.includes("Unsigned(u64)"),
          "SurfaceDataValue must preserve unsigned integers",
        );
        assertCondition(
          manifest.surface_data_value_variants.includes(
            "Object(SortedUniqueDisplayTextKeysIncludingEmpty)",
          ),
          "SurfaceDataValue object keys must be sorted and unique",
        );
      },
    ],
    [
      "released JSONL pagination filtering metadata and turn-control decoder behavior is exact",
      (manifest) => {
        assertContractKeys(
          manifest.jsonl_pagination_filter_contract,
          ["filter_rules", "cursor_binding", "client_bounds"],
          "jsonl_pagination_filter_contract",
        );
        assertCondition(
          manifest.jsonl_legacy_turn_control_vectors.length > 0 &&
            manifest.jsonl_history_query_vectors.length > 0,
          "JSONL pagination or control vectors are empty",
        );
      },
    ],
    [
      "released JSONL has exactly 38 request spellings and 66 serialized event discriminators split into 41 runtime/history/error/turn and 25 service events",
      (manifest) => {
        assertCondition(manifest.jsonl_request_inventory.length === 38, "JSONL requests must be 38");
        assertCondition(manifest.jsonl_event_inventory.length === 41, "JSONL events must be 41");
        assertCondition(
          manifest.jsonl_preserved_service_event_names.length === 25,
          "JSONL service events must be 25",
        );
      },
    ],
    [
      "SurfaceCommitBatch is the sole public linearization unit and every batch passes event/byte preflight before WAL/source mutation/receipt",
      (manifest) => {
        const limits = manifest.closed_inventory.surface_commit_batch_limits;
        assertCondition(limits.event_limit === 1024, "surface batch event limit must be 1024");
        assertCondition(
          limits.canonical_encoded_byte_limit === 8_388_608,
          "surface batch canonical byte limit must be 8388608",
        );
        assertContractText(
          manifest.surface_snapshot_contract,
          ["SurfaceCommitBatch", "precommit", "WAL"],
          "surface snapshot contract",
        );
      },
    ],
    [
      "cursor attach accepts only complete batch boundaries and replay/subscription items are complete batches or typed gaps/seals",
      (manifest) => {
        assertContractText(
          manifest.attach_contract,
          ["complete batch boundary", "SurfaceCommitBatch", "Gap", "Sealed"],
          "attach contract",
        );
        assertExactArray(
          manifest.closed_inventory.surface_commit_batch_variants,
          ["Batch", "Gap", "Sealed"],
          "surface commit batch variants",
        );
      },
    ],
    [
      "Goal continuation Admit and Stop decisions are atomic with GenerationStopped and no stopped-without-successor/finalization state is recoverable",
      (manifest) => {
        assertContractKeys(
          manifest.goal_stop_contract,
          ["admit_batch", "stop_batch", "recovery", "terminal_rule"],
          "goal_stop_contract",
        );
        assertContractText(
          manifest.goal_stop_contract,
          ["GenerationStopped"],
          "goal stop contract",
        );
      },
    ],
    [
      "Goal recovery closes stale run with exact closed-run receipt and current_run=None; no automatic admission",
      (manifest) =>
        assertContractText(
          manifest.goal_stop_contract.recovery,
          ["current_run=None", "never auto-admits"],
          "goal recovery contract",
        ),
    ],
    [
      "every DeferredMutation carries one closed DeferredRepair variant with exact state/token equality and nonempty missing work",
      (manifest) => {
        assertCondition(
          manifest.deferred_state_repair_matrix.length === 10,
          "deferred repair matrix must have 10 rows",
        );
        assertCondition(
          manifest.deferred_state_nonempty_invariants.length === 5,
          "deferred nonempty invariants must have 5 rows",
        );
        assertContractText(
          manifest.deferred_state_nonempty_invariants,
          ["nonempty", "byte-for-byte"],
          "deferred nonempty invariants",
        );
      },
    ],
    [
      "FinalizingDegraded MissingFinalization accepts only RetryFinalization while TerminalProjectionPending accepts only local RetryProjection",
      (manifest) => {
        const text = JSON.stringify(manifest.deferred_state_repair_matrix);
        assertCondition(
          text.includes("MissingFinalization") &&
            text.includes("RetryFinalization") &&
            text.includes("TerminalProjectionPending") &&
            text.includes("RetryProjection"),
          "FinalizingDegraded repair routing is incomplete",
        );
      },
    ],
    [
      "shutdown plan/output/ack values are a sorted exact bijection; recorded-only catalog receipts and final HostLifecycle ordering are enforced",
      (manifest) => {
        assertContractKeys(
          manifest.shutdown_contract,
          ["plan", "output_bijection", "close_receipts", "zero_thread_host"],
          "shutdown_contract",
        );
        assertContractText(
          manifest.shutdown_contract,
          ["HostLifecycle", "recorded"],
          "shutdown contract",
        );
      },
    ],
    [
      "native interaction answers contain no authority; bound runtime injects route/grant/response/fingerprint and legacy JSONL policies are capability-bound",
      (manifest) => {
        assertContractKeys(
          manifest.interaction_answer_contract,
          ["client_type", "bound_type", "authority_injection", "policies"],
          "interaction_answer_contract",
        );
        assertContractText(
          manifest.interaction_answer_contract,
          ["route", "grant", "response", "AuthorityFingerprint"],
          "interaction answer contract",
        );
      },
    ],
    [
      "JSONL permission/respond enters JsonlOpaquePermissionRouter; safe receipt tombstones replay only identical keyed identity and write failure retires routes",
      (manifest) => {
        assertContractText(
          manifest.jsonl_permission_router_vectors,
          ["safe_decision_scope", "same_id_same_digest", "write", "retire"],
          "JSONL permission router vectors",
        );
        const route = manifest.jsonl_routing_matrix.find((row) => row[0] === "permission/respond");
        assertCondition(route?.[1] === "OpaquePermissionRouter", "JSONL permission route owner drifted");
      },
    ],
    [
      "ACP ToolApproval projection is total only for exact AllowOnce/RejectOnce options and never fabricates Thread/Operation scope",
      (manifest) => {
        const row = manifest.acp_projection_matrix.find(
          (candidate) => candidate[0] === "Interaction.ToolApproval",
        );
        assertContractText(
          row,
          ["AllowOnce", "RejectOnce", "no Thread/Operation"],
          "ACP ToolApproval projection",
        );
      },
    ],
    [
      "typed session/thread/MCP/input cursors use signed opaque authenticators; LegacyJsonl offset cursor is the sole compatibility exception",
      (manifest) => {
        assertContractText(
          manifest.closed_inventory.surface_cursor_authenticator_domains,
          ["Session", "Thread", "Mcp", "Input"],
          "surface cursor domains",
        );
        assertContractKeys(
          manifest.legacy_jsonl_offset_cursor_contract,
          ["fields", "parse_rules", "page_rules"],
          "legacy JSONL offset cursor contract",
        );
      },
    ],
    [
      "MCP malformed descriptors have one explicit diagnostic/omission disposition and cannot become binding authority",
      (manifest) =>
        assertContractText(
          manifest.mcp_catalog_query_contract.negative,
          ["adapter-side miss encoding"],
          "MCP malformed descriptor disposition",
        ),
    ],
    [
      "operation patch inventory has exactly 21 variants including input failure, suspension rebase, and finalization settlement",
      (manifest) => {
        const variants = manifest.closed_inventory.operation_patch_variants;
        assertCondition(variants.length === 21, "operation patch inventory must have 21 variants");
        for (const required of [
          "InputBindingsFailed",
          "SuspensionRebasedAfterUnstartedResume",
          "FinalizationSettlementRecorded",
        ]) {
          assertCondition(variants.includes(required), `operation patch inventory is missing ${required}`);
        }
      },
    ],
    [
      "operation transition inventory has exactly 11 legal edges including suspension rebase and terminal projection repair",
      (manifest) => {
        assertCondition(manifest.operation_transitions.length === 11, "operation transitions must have 11 edges");
        assertContractText(
          manifest.operation_transitions,
          ["SuspensionRebasedAfterUnstartedResume", "RetryProjection"],
          "operation transitions",
        );
      },
    ],
    [
      "operation terminal mapping has exactly 65 exhaustive rows and contains no impossible Completed(Failed) or ExecutionFailed(Verification|Persistence) source",
      (manifest) => {
        assertCondition(
          manifest.operation_terminal_mapping.length === 65,
          "operation terminal mapping must contain 65 rows",
        );
        for (const row of manifest.operation_terminal_mapping) {
          if (
            row[0] === "Completed(Failed)" ||
            row[0] === "ExecutionFailed(Verification)" ||
            row[0] === "ExecutionFailed(Persistence)"
          ) {
            fail(`operation terminal mapping contains impossible source ${row[0]}`);
          }
        }
      },
    ],
    [
      "Task, Workflow run, Workflow phase, Workflow agent attempt, and Subagent transitions are closed and terminal states are absorbing",
      (manifest) => {
        for (const [table, terminals] of Object.entries(TERMINAL_TRANSITION_STATES)) {
          assertCondition(
            manifest[table].every((row) => !terminals.includes(row[0].split("(")[0])),
            `${table} has a non-absorbing terminal state`,
          );
        }
      },
    ],
    [
      "ACP prompt binding states and transitions are closed; reservation and terminal cursor witnesses precede bound cancellation and response writing",
      (manifest) => {
        assertExactArray(
          manifest.acp_prompt_binding_states,
          [
            "Decoded",
            "Reserved",
            "Bound",
            "TerminalGated",
            "ResponseWriting",
            "Completed",
            "TransportRetired",
          ],
          "ACP prompt binding states",
        );
        assertCondition(
          manifest.acp_prompt_binding_transitions.length === 10,
          "ACP prompt binding transitions must have 10 rows",
        );
        assertContractText(
          manifest.acp_prompt_binding_transitions,
          ["reservation", "terminal", "cursor", "response"],
          "ACP prompt binding transitions",
        );
      },
    ],
    [
      "ACP capability results pass a 4 MiB canonical construction limit below the 8 MiB surface batch limit",
      (manifest) => {
        const capability = manifest.acp_capability_result_size_contract.canonical_byte_limit;
        const batch = manifest.closed_inventory.surface_commit_batch_limits.canonical_encoded_byte_limit;
        assertCondition(capability === 4_194_304, "ACP capability limit must be 4 MiB");
        assertCondition(batch === 8_388_608 && capability < batch, "ACP limit must remain below batch limit");
      },
    ],
    [
      "JSONL supervisor close, sole-owner routing, bounded live admission with typed rejection settlement, direct responder, runtime durable repair transfer, exact repair/fixed-service settlement coverage, single typed IO failure, and lookup/insertion tombstone machines are closed",
      (manifest) => {
        assertCondition(manifest.jsonl_supervisor_close_matrix.length === 6, "JSONL close matrix is open");
        assertCondition(manifest.jsonl_routing_matrix.length === 10, "JSONL routing matrix is open");
        for (const contract of [
          "jsonl_live_request_contract",
          "jsonl_direct_responder_contract",
          "jsonl_runtime_durable_repair_contract",
          "jsonl_committed_repair_contract",
          "jsonl_service_settlement_contract",
          "jsonl_supervisor_failure_identity_contract",
          "jsonl_tombstone_contract",
        ]) {
          assertCondition(Object.keys(manifest[contract]).length > 0, `${contract} must not be empty`);
        }
      },
    ],
    [
      "generic repair exact retries reconstruct the original typed value, receipts, and waiter without rerunning effects",
      (manifest) => {
        assertContractKeys(
          manifest.generic_repair_replay_contract,
          ["conflict", "effect_rule", "ordering", "success"],
          "generic_repair_replay_contract",
        );
        assertContractText(
          manifest.generic_repair_replay_contract,
          ["original", "receipt", "waiter"],
          "generic repair replay contract",
        );
      },
    ],
    [
      "shutdown repair returns retained exact CloseThread or ShutdownHost output after surface sealing",
      (manifest) => {
        assertExactArray(
          manifest.retained_shutdown_output_variants,
          ["CloseThread(CloseThreadOutput)", "ShutdownHost(ShutdownHostOutput)"],
          "retained shutdown output variants",
        );
        assertContractText(
          manifest.shutdown_contract.repair,
          ["retained", "never rescans"],
          "shutdown repair contract",
        );
      },
    ],
  ]);
}

function validateNamedInvariants(manifest, reviewedManifest) {
  const invariants = requireArray(
    manifest.phase_0a_manifest_invariants,
    "phase_0a_manifest_invariants",
  );
  assertUnique(invariants, "phase_0a_manifest_invariants");
  const registry = invariantRegistry();
  for (const invariant of invariants) {
    if (!registry.has(invariant)) fail(`unknown phase_0a_manifest_invariant: ${invariant}`);
  }
  for (const invariant of registry.keys()) {
    if (!invariants.includes(invariant)) fail(`missing phase_0a_manifest_invariant: ${invariant}`);
  }
  assertExactArray(invariants, [...registry.keys()], "phase_0a_manifest_invariants order");
  assertCondition(registry.size === 61, "all 61 invariant handlers must be registered");
  const bindings = [];
  for (const invariant of invariants) {
    const paths = new Set();
    registry.get(invariant)(tracedManifest(manifest, paths));
    assertCondition(paths.size > 0, `invariant has no reviewed path binding: ${invariant}`);
    bindings.push([invariant, paths]);
  }

  const gate = requireObject(manifest.phase_0b_gate, "phase_0b_gate");
  assertCondition(
    gate.requires_written_review_of_this_exact_manifest === true,
    "phase_0b_gate must require exact-manifest written review",
  );
  assertCondition(
    gate.production_code_authorized === false,
    "phase_0b_gate must keep production code unauthorized",
  );
  if (reviewedManifest) {
    for (const [invariant, paths] of bindings) {
      for (const pathName of paths) {
        assertReviewedFragment(manifest, reviewedManifest, pathName, `invariant ${invariant}`);
      }
    }
  }
}

export function validateManifestStructure(manifest, { reviewedManifest } = {}) {
  requireObject(manifest, "manifest");
  if (reviewedManifest !== undefined) requireObject(reviewedManifest, "reviewedManifest");
  if (manifest.schema_version !== 1) fail("schema_version must be 1");
  requireNonemptyString(manifest.contract_version, "contract_version");
  requireNonemptyString(manifest.normative_document, "normative_document");
  validateTables(manifest);
  validateClosedInventories(manifest);
  validateRuntimeSurfacePublicExportManifest(manifest);
  validateCommands(manifest, "thread_commands", "surface_commands", "thread_command_dispositions");
  validateCommands(manifest, "host_commands", "surface_host_commands", "host_dispositions");
  validateAcpDispositions(manifest);
  validateAcknowledgementForms(manifest);
  validateTransitions(manifest);
  validateTestGenerators(manifest);
  validateNamedInvariants(manifest, reviewedManifest);
  if (reviewedManifest && canonicalJson(manifest) !== canonicalJson(reviewedManifest)) {
    fail("reviewed manifest whole-document drift outside invariant bindings");
  }
}

function git(repoRoot, args, options = {}) {
  return execFileSync("git", args, {
    cwd: repoRoot,
    encoding: options.encoding ?? "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}

function relativeRepoPath(repoRoot, absolutePath) {
  const relative = path.relative(repoRoot, absolutePath).split(path.sep).join("/");
  if (relative.startsWith("../") || relative === "..") {
    fail(`${absolutePath} is outside repository root`);
  }
  return relative;
}

function checkedRepoFile(repoRoot, relativePath, label) {
  requireNonemptyString(relativePath, label);
  const absolutePath = path.resolve(repoRoot, relativePath);
  if (relativeRepoPath(repoRoot, absolutePath).startsWith("../") || !existsSync(absolutePath)) {
    fail(`${label} does not exist: ${relativePath}`);
  }
  return absolutePath;
}

export function validateArtifactBundle(manifest, { repoRoot }) {
  const bundle = requireObject(manifest.artifact_bundle, "artifact_bundle");
  const privatePath = checkedRepoFile(
    repoRoot,
    bundle.private_contract_path,
    "private contract path",
  );
  if (manifest.normative_document !== bundle.private_contract_path) {
    fail("normative_document does not match artifact_bundle.private_contract_path");
  }
  const privateHash = canonicalSha256(readFileSync(privatePath));
  if (privateHash !== bundle.private_contract_sha256) {
    fail("private contract SHA-256 mismatch");
  }

  const designPath = checkedRepoFile(repoRoot, bundle.parent_design_path, "parent design path");
  if (manifest.baseline.design_commit !== bundle.parent_design_commit) {
    fail("parent design commit backlink mismatch");
  }
  if (manifest.baseline.design_blob !== bundle.parent_design_blob) {
    fail("parent design blob backlink mismatch");
  }
  const committedBlob = git(repoRoot, [
    "rev-parse",
    `${bundle.parent_design_commit}:${bundle.parent_design_path}`,
  ]).trim();
  if (committedBlob !== bundle.parent_design_blob) fail("parent design git blob mismatch");
  const currentBlob = git(repoRoot, ["hash-object", designPath]).trim();
  if (currentBlob !== bundle.parent_design_blob) fail("current parent design blob mismatch");
  const currentDesignHash = canonicalSha256(readFileSync(designPath));
  if (currentDesignHash !== bundle.parent_design_sha256) {
    fail("parent design SHA-256 mismatch");
  }
  const committedDesign = git(
    repoRoot,
    ["show", `${bundle.parent_design_commit}:${bundle.parent_design_path}`],
    { encoding: "buffer" },
  );
  if (canonicalSha256(committedDesign) !== bundle.parent_design_sha256) {
    fail("parent design commit SHA-256 backlink mismatch");
  }
}

export function validateArtifactDigest(digest, { repoRoot, sourceOverrides }) {
  requireObject(digest, "artifact digest");
  if (digest.schema_version !== 1) fail("artifact digest schema_version must be 1");
  if (digest.algorithm !== "sha256") fail("artifact digest algorithm must be sha256");
  const artifacts = requireArray(digest.artifacts, "artifact digest artifacts");
  assertExactArray(
    artifacts.map((artifact) => artifact.path),
    REVIEWED_ARTIFACT_PATHS,
    "artifact digest paths",
  );
  for (const [index, artifact] of artifacts.entries()) {
    requireObject(artifact, `artifact digest row ${index}`);
    const artifactPath = requireNonemptyString(
      artifact.path,
      `artifact digest row ${index} path`,
    );
    if (!/^[0-9a-f]{64}$/.test(artifact.sha256 ?? "")) {
      fail(`artifact digest row ${index} must contain a lowercase SHA-256`);
    }
    const bytes = sourceOverrides?.has(artifactPath)
      ? Buffer.from(sourceOverrides.get(artifactPath), "utf8")
      : readFileSync(checkedRepoFile(repoRoot, artifactPath, `${artifactPath} digest source`));
    if (canonicalSha256(bytes) !== artifact.sha256) {
      fail(`artifact digest SHA-256 mismatch for ${artifactPath}`);
    }
  }
}

function rustCharLiteralEnd(source, start) {
  if (source[start] !== "'") return undefined;
  let index = start + 1;
  if (source[index] === "\\") {
    index += 1;
    const escape = source[index];
    if (["0", "n", "r", "t", "\\", "'", '"'].includes(escape)) {
      index += 1;
    } else if (escape === "x" && /^[0-9A-Fa-f]{2}/.test(source.slice(index + 1))) {
      index += 3;
    } else if (escape === "u" && source[index + 1] === "{") {
      const close = source.indexOf("}", index + 2);
      const digits = source.slice(index + 2, close).replaceAll("_", "");
      if (close < 0 || !/^[0-9A-Fa-f]{1,6}$/.test(digits)) {
        return undefined;
      }
      index = close + 1;
    } else {
      return undefined;
    }
  } else {
    const codePoint = source.codePointAt(index);
    if (codePoint === undefined) return undefined;
    const character = String.fromCodePoint(codePoint);
    if (["'", "\\", "\n", "\r", "\t"].includes(character)) return undefined;
    index += character.length;
  }
  return source[index] === "'" ? index + 1 : undefined;
}

function stripRustComments(source) {
  let output = "";
  let state = "code";
  let blockDepth = 0;
  let escaped = false;
  for (let index = 0; index < source.length; index += 1) {
    const character = source[index];
    const next = source[index + 1];
    if (state === "line-comment") {
      if (character === "\n") {
        output += character;
        state = "code";
      }
      continue;
    }
    if (state === "block-comment") {
      if (character === "/" && next === "*") {
        blockDepth += 1;
        index += 1;
      } else if (character === "*" && next === "/") {
        blockDepth -= 1;
        index += 1;
        if (blockDepth === 0) state = "code";
      } else if (character === "\n") {
        output += character;
      }
      continue;
    }
    if (state === "string") {
      output += character;
      if (escaped) escaped = false;
      else if (character === "\\") escaped = true;
      else if (character === '"') state = "code";
      continue;
    }
    if (character === "/" && next === "/") {
      state = "line-comment";
      index += 1;
    } else if (character === "/" && next === "*") {
      state = "block-comment";
      blockDepth = 1;
      index += 1;
    } else {
      output += character;
      if (character === '"') state = "string";
      else if (character === "'") {
        const end = rustCharLiteralEnd(source, index);
        if (end !== undefined) {
          output += source.slice(index + 1, end);
          index = end - 1;
        }
      }
    }
  }
  return output;
}

function variantName(chunk) {
  let remaining = chunk.trim();
  while (remaining.startsWith("#[")) {
    let depth = 0;
    let end = -1;
    let quote;
    let escaped = false;
    for (let index = 1; index < remaining.length; index += 1) {
      const character = remaining[index];
      if (quote) {
        if (escaped) escaped = false;
        else if (character === "\\") escaped = true;
        else if (character === quote) quote = undefined;
        continue;
      }
      if (character === '"') {
        quote = character;
      } else if (character === "'") {
        const charEnd = rustCharLiteralEnd(remaining, index);
        if (charEnd !== undefined) index = charEnd - 1;
      } else if (character === "[") depth += 1;
      else if (character === "]") {
        depth -= 1;
        if (depth === 0) {
          end = index;
          break;
        }
      }
    }
    if (end < 0) fail("unterminated Rust enum attribute");
    remaining = remaining.slice(end + 1).trimStart();
  }
  return remaining.match(/^([A-Za-z][A-Za-z0-9_]*)/)?.[1];
}

export function parseRustEnum(source, declaration) {
  const uncommented = stripRustComments(source);
  const declarationIndex = uncommented.indexOf(declaration);
  if (declarationIndex < 0) fail(`missing Rust enum declaration ${declaration}`);
  const bodyStart = declarationIndex + declaration.length;
  const variants = [];
  let chunk = "";
  let braceDepth = 0;
  let parenDepth = 0;
  let bracketDepth = 0;
  let stringDelimiter;
  let escaped = false;
  const pushChunk = () => {
    const name = variantName(chunk);
    if (name) variants.push(name);
    chunk = "";
  };
  const body = uncommented.slice(bodyStart);
  for (let index = 0; index < body.length; index += 1) {
    const character = body[index];
    if (stringDelimiter) {
      chunk += character;
      if (escaped) escaped = false;
      else if (character === "\\") escaped = true;
      else if (character === stringDelimiter) stringDelimiter = undefined;
      continue;
    }
    if (character === '"') {
      stringDelimiter = character;
      chunk += character;
      continue;
    }
    if (character === "'") {
      const charEnd = rustCharLiteralEnd(body, index);
      if (charEnd !== undefined) {
        chunk += body.slice(index, charEnd);
        index = charEnd - 1;
        continue;
      }
    }
    if (
      character === "}" &&
      braceDepth === 0 &&
      parenDepth === 0 &&
      bracketDepth === 0
    ) {
      pushChunk();
      break;
    }
    if (character === "{") braceDepth += 1;
    else if (character === "}" && braceDepth > 0) braceDepth -= 1;
    else if (character === "(") parenDepth += 1;
    else if (character === ")") parenDepth -= 1;
    else if (character === "[") bracketDepth += 1;
    else if (character === "]") bracketDepth -= 1;
    if (
      character === "," &&
      braceDepth === 0 &&
      parenDepth === 0 &&
      bracketDepth === 0
    ) {
      pushChunk();
    } else {
      chunk += character;
    }
  }
  return variants;
}

export function parseRuntimeSurfacePublicExports(source) {
  const code = maskRustNonCode(source);
  const exportsByModule = {};
  const declarations = [...code.matchAll(/\bpub\s+use\s+([a-z_][a-z0-9_]*)\s*::/g)];
  for (const declaration of declarations) {
    const moduleName = declaration[1];
    const bodyStart = declaration.index + declaration[0].length;
    const declarationEnd = code.indexOf(";", bodyStart);
    if (declarationEnd < 0) fail(`unterminated public export for ${moduleName}`);
    const body = code.slice(bodyStart, declarationEnd).trim();
    if (body === "*") {
      fail(`runtime-surface public exports must be explicit; found pub use ${moduleName}::*`);
    }
    if (exportsByModule[moduleName]) {
      fail(`runtime-surface module ${moduleName} has more than one public export declaration`);
    }
    const names = (body.startsWith("{") && body.endsWith("}")
      ? body.slice(1, -1)
      : body
    )
      .split(",")
      .map((name) => name.trim())
      .filter(Boolean);
    for (const name of names) {
      if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
        fail(`runtime-surface public export ${moduleName}::${name} is not an exact identifier`);
      }
    }
    assertUnique(names, `runtime-surface ${moduleName} public exports`);
    exportsByModule[moduleName] = names;
  }
  return exportsByModule;
}

export function parseSurfaceFacadeExports(source) {
  const code = maskRustNonCode(source);
  const declaration = /\bpub\s+use\s+crate::runtime_surface::\s*\{/.exec(code);
  if (!declaration) {
    if (/\bpub\s+use\s+crate::runtime_surface::\s*\*/.test(code)) {
      fail("surface facade exports must be explicit");
    }
    fail("surface facade export declaration is missing");
  }
  const bodyStart = declaration.index + declaration[0].lastIndexOf("{");
  const bodyEnd = matchingBraceEnd(code, bodyStart, "surface facade export");
  const names = code
    .slice(bodyStart + 1, bodyEnd - 1)
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  for (const name of names) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
      fail(`surface facade export ${name} is not an exact identifier`);
    }
  }
  assertUnique(names, "surface facade exports");
  return names;
}

export function assertNoProductionRuntimeSurfaceSiblingGlobs(source, moduleName) {
  const production = maskCfgTestItems(source);
  for (const declaration of rustUseDeclarations(production)) {
    if (declaration.path === "super::*") {
      fail(`runtime-surface ${moduleName} production imports must be explicit; found use super::*`);
    }
  }
}

function readRepoSource(repoRoot, relativePath, sourceOverrides) {
  const normalized = relativePath.split(path.sep).join("/");
  if (sourceOverrides?.has(normalized)) return sourceOverrides.get(normalized);
  return readFileSync(checkedRepoFile(repoRoot, normalized, `${normalized} source`), "utf8");
}

function validateSourceReference(repoRoot, reference, label, sourceOverrides) {
  const match = requireNonemptyString(reference, label).match(/^(.*):(\d+)(?:-(\d+))?$/);
  if (!match) fail(`${label} is not a path:line reference: ${reference}`);
  const relativePath = match[1].split(path.sep).join("/");
  checkedRepoFile(repoRoot, relativePath, label);
  const lines = readRepoSource(repoRoot, relativePath, sourceOverrides).split(/\r?\n/);
  const lineCount = lines.length;
  const first = Number(match[2]);
  const last = Number(match[3] ?? match[2]);
  if (first < 1 || last < first || last > lineCount) {
    fail(`${label} is outside ${match[1]}: ${reference}`);
  }
  return {
    relativePath,
    snippet: lines.slice(first - 1, last).join("\n"),
    source: lines.join("\n"),
  };
}

function rustSourcePaths(repoRoot, sourceRoots) {
  const paths = [];
  const visit = (directory) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const absolute = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(absolute);
      else if (entry.isFile() && entry.name.endsWith(".rs")) {
        paths.push(relativeRepoPath(repoRoot, absolute));
      }
    }
  };
  for (const sourceRoot of sourceRoots) visit(path.join(repoRoot, sourceRoot));
  return paths.sort();
}

function tuiRustSourcePaths(repoRoot) {
  return rustSourcePaths(repoRoot, ["crates/orca-tui/src"]);
}

export function unstableSurfaceReferenceLines(source) {
  const code = maskRustNonCode(source);
  const references = [];
  for (const match of code.matchAll(/\bunstable_surface\b/g)) {
    references.push(code.slice(0, match.index).split("\n").length);
  }
  return references;
}

function validateNoUnstableSurfaceReferences(repoRoot, sourceOverrides) {
  const references = [];
  for (const relativePath of rustSourcePaths(repoRoot, UNSTABLE_SURFACE_SOURCE_ROOTS)) {
    for (const line of unstableSurfaceReferenceLines(
      readRepoSource(repoRoot, relativePath, sourceOverrides),
    )) {
      references.push(`${relativePath}:${line}`);
    }
  }
  if (references.length > 0) {
    fail(`unstable_surface references must be removed:\n${references.join("\n")}`);
  }
}

function cfgPredicateTokens(predicate) {
  const tokens = [];
  let index = 0;
  while (index < predicate.length) {
    const character = predicate[index];
    if (/\s/.test(character)) {
      index += 1;
      continue;
    }
    if ("(),=".includes(character)) {
      tokens.push(character);
      index += 1;
      continue;
    }
    if (character === '"') {
      let end = index + 1;
      let escaped = false;
      while (end < predicate.length) {
        const current = predicate[end];
        if (!escaped && current === '"') break;
        if (!escaped && current === "\\") escaped = true;
        else escaped = false;
        end += 1;
      }
      if (end >= predicate.length) return null;
      tokens.push(predicate.slice(index, end + 1));
      index = end + 1;
      continue;
    }
    const atom = predicate.slice(index).match(/^[A-Za-z_][A-Za-z0-9_-]*/)?.[0];
    if (!atom) return null;
    tokens.push(atom);
    index += atom.length;
  }
  return tokens;
}

function parseCfgPredicate(predicate) {
  const tokens = cfgPredicateTokens(predicate);
  if (!tokens) return null;
  let index = 0;

  const parseNode = () => {
    const name = tokens[index];
    if (!name || "(),=".includes(name)) return null;
    index += 1;
    if (["all", "any", "not"].includes(name) && tokens[index] === "(") {
      index += 1;
      const children = [];
      if (tokens[index] !== ")") {
        while (true) {
          const child = parseNode();
          if (!child) return null;
          children.push(child);
          if (tokens[index] === ")") break;
          if (tokens[index] !== ",") return null;
          index += 1;
        }
      }
      index += 1;
      if (name === "not" && children.length !== 1) return null;
      return { kind: name, children };
    }

    const parts = [name];
    while (index < tokens.length && ![",", ")"].includes(tokens[index])) {
      parts.push(tokens[index]);
      index += 1;
    }
    return { kind: "atom", name: parts.join("") };
  };

  const result = parseNode();
  return result && index === tokens.length ? result : null;
}

function evaluateCfgPredicate(node, assignments) {
  if (node.kind === "atom") return assignments.get(node.name);
  const values = node.children.map((child) => evaluateCfgPredicate(child, assignments));
  if (node.kind === "not") {
    return values[0] === undefined ? undefined : !values[0];
  }
  if (node.kind === "all") {
    if (values.includes(false)) return false;
    return values.every((value) => value === true) ? true : undefined;
  }
  if (values.includes(true)) return true;
  return values.every((value) => value === false) ? false : undefined;
}

function cfgPredicateImpliesTest(predicate) {
  const node = parseCfgPredicate(predicate);
  if (!node) return false;
  const atoms = new Set();
  const collectAtoms = (current) => {
    if (current.kind === "atom") atoms.add(current.name);
    else current.children.forEach(collectAtoms);
  };
  collectAtoms(node);
  const assignments = new Map([["test", false]]);

  const canBeTrueWithoutTest = () => {
    const value = evaluateCfgPredicate(node, assignments);
    if (value !== undefined) return value;
    const atom = [...atoms].find((name) => !assignments.has(name));
    if (!atom) return false;
    assignments.set(atom, false);
    if (canBeTrueWithoutTest()) {
      assignments.delete(atom);
      return true;
    }
    assignments.set(atom, true);
    const result = canBeTrueWithoutTest();
    assignments.delete(atom);
    return result;
  };

  return !canBeTrueWithoutTest();
}

function cfgAttributes(source) {
  const code = maskRustNonCode(source);
  const attributes = [];
  const startPattern = /#\s*\[\s*cfg\s*\(/g;
  for (const match of code.matchAll(startPattern)) {
    const predicateStart = match.index + match[0].length;
    let depth = 1;
    let index = predicateStart;
    while (index < code.length && depth > 0) {
      if (code[index] === "(") depth += 1;
      else if (code[index] === ")") depth -= 1;
      index += 1;
    }
    if (depth !== 0) continue;
    let end = index;
    while (/\s/.test(code[end] ?? "")) end += 1;
    if (code[end] !== "]") continue;
    attributes.push({
      start: match.index,
      end: end + 1,
      predicate: source.slice(predicateStart, index - 1),
    });
  }
  return attributes;
}

function rustFileModuleDirectory(relativePath) {
  const directory = path.posix.dirname(relativePath);
  const basename = path.posix.basename(relativePath);
  if (["lib.rs", "main.rs", "mod.rs"].includes(basename)) return directory;
  return path.posix.join(directory, basename.slice(0, -".rs".length));
}

function inlineModuleDirectoryAt(source, code, relativePath, targetIndex) {
  const scopes = [];
  for (let index = 0; index < targetIndex; index += 1) {
    if (code[index] === "{") {
      const boundary = Math.max(
        code.lastIndexOf(";", index - 1),
        code.lastIndexOf("{", index - 1),
        code.lastIndexOf("}", index - 1),
      );
      const codeHeader = code.slice(boundary + 1, index);
      const moduleName = codeHeader.match(
        /(?:^|\s)(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z][A-Za-z0-9_]*)\s*$/,
      )?.[1];
      let moduleDirectory;
      if (moduleName) {
        const parentDirectory = scopes.findLast(Boolean);
        const explicitPath = source
          .slice(boundary + 1, index)
          .match(/#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]/)?.[1];
        const baseDirectory =
          parentDirectory ??
          (explicitPath === undefined
            ? rustFileModuleDirectory(relativePath)
            : path.posix.dirname(relativePath));
        moduleDirectory = path.posix.normalize(
          path.posix.join(baseDirectory, explicitPath ?? moduleName),
        );
      }
      scopes.push(moduleDirectory);
    } else if (code[index] === "}") {
      scopes.pop();
    }
  }
  return scopes.findLast(Boolean);
}

function cfgTestExternalModulePaths(repoRoot, sourcePaths, sourceOverrides) {
  const excluded = new Set();
  const available = new Set(sourcePaths);
  for (const relativePath of sourcePaths) {
    const source = stripRustComments(readRepoSource(repoRoot, relativePath, sourceOverrides));
    const code = maskRustNonCode(source);
    for (const attribute of cfgAttributes(source)) {
      if (!cfgPredicateImpliesTest(attribute.predicate)) continue;
      const declaration = code
        .slice(attribute.end)
        .match(
          /^\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z][A-Za-z0-9_]*)\s*;/,
        );
      if (!declaration) continue;
      const declarationSource = source.slice(
        attribute.end,
        attribute.end + declaration.index + declaration[0].length,
      );
      const explicitPath = declarationSource.match(/#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]/)?.[1];
      const moduleName = declaration[1];
      const inlineDirectory = inlineModuleDirectoryAt(
        source,
        code,
        relativePath,
        attribute.start,
      );
      const implicitDirectory =
        inlineDirectory ?? rustFileModuleDirectory(relativePath);
      const explicitDirectory =
        inlineDirectory ?? path.posix.dirname(relativePath);
      const fileCandidate = path.posix.normalize(
        path.posix.join(
          explicitPath === undefined ? implicitDirectory : explicitDirectory,
          explicitPath ?? `${moduleName}.rs`,
        ),
      );
      const directoryCandidate = path.posix.join(
        implicitDirectory,
        moduleName,
        "mod.rs",
      );
      if (available.has(fileCandidate)) excluded.add(fileCandidate);
      else if (explicitPath === undefined && available.has(directoryCandidate)) {
        excluded.add(directoryCandidate);
      }
    }
  }
  return excluded;
}

function maskRustNonCode(source) {
  const cached = rustNonCodeMaskCache.get(source);
  if (cached !== undefined) return cached;
  let output = "";
  let index = 0;
  let state = "code";
  let blockDepth = 0;
  let escaped = false;
  let rawTerminator = "";
  const masked = (text) => text.replace(/[^\n]/g, " ");

  while (index < source.length) {
    const character = source[index];
    const next = source[index + 1];
    if (state === "code") {
      if (character === "/" && next === "/") {
        output += "  ";
        index += 2;
        state = "line-comment";
        continue;
      }
      if (character === "/" && next === "*") {
        output += "  ";
        index += 2;
        blockDepth = 1;
        state = "block-comment";
        continue;
      }
      const rawStart = source.slice(index).match(/^r(#+)?"/);
      if (rawStart) {
        output += masked(rawStart[0]);
        index += rawStart[0].length;
        rawTerminator = `"${rawStart[1] ?? ""}`;
        state = "raw-string";
        continue;
      }
      if (character === '"') {
        output += " ";
        index += 1;
        escaped = false;
        state = "string";
        continue;
      }
      if (character === "'") {
        const end = rustCharLiteralEnd(source, index);
        if (end !== undefined) {
          output += masked(source.slice(index, end));
          index = end;
          continue;
        }
      }
      output += character;
      index += 1;
      continue;
    }
    if (state === "line-comment") {
      output += character === "\n" ? "\n" : " ";
      index += 1;
      if (character === "\n") state = "code";
      continue;
    }
    if (state === "block-comment") {
      if (character === "/" && next === "*") {
        output += "  ";
        index += 2;
        blockDepth += 1;
      } else if (character === "*" && next === "/") {
        output += "  ";
        index += 2;
        blockDepth -= 1;
        if (blockDepth === 0) state = "code";
      } else {
        output += character === "\n" ? "\n" : " ";
        index += 1;
      }
      continue;
    }
    if (state === "raw-string") {
      if (source.startsWith(rawTerminator, index)) {
        output += masked(rawTerminator);
        index += rawTerminator.length;
        state = "code";
      } else {
        output += character === "\n" ? "\n" : " ";
        index += 1;
      }
      continue;
    }
    output += character === "\n" ? "\n" : " ";
    index += 1;
    if (escaped) escaped = false;
    else if (character === "\\") escaped = true;
    else if (character === '"') state = "code";
  }
  rustNonCodeMaskCache.set(source, output);
  return output;
}

function matchingBraceEnd(source, bodyStart, label = "Rust item") {
  let depth = 0;
  for (let index = bodyStart; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    else if (source[index] === "}") {
      depth -= 1;
      if (depth === 0) return index + 1;
    }
  }
  fail(`unterminated ${label} body`);
}

function itemBodyStart(source, itemStart) {
  let parentheses = 0;
  let brackets = 0;
  for (let index = itemStart; index < source.length; index += 1) {
    const character = source[index];
    if (character === "(") parentheses += 1;
    else if (character === ")") parentheses = Math.max(0, parentheses - 1);
    else if (character === "[") brackets += 1;
    else if (character === "]") brackets = Math.max(0, brackets - 1);
    else if (parentheses === 0 && brackets === 0) {
      if (character === ";") return undefined;
      if (character === "{") return index;
    }
  }
  return undefined;
}

function skipRustAttributes(source, start) {
  let index = start;
  while (index < source.length) {
    while (/\s/.test(source[index] ?? "")) index += 1;
    if (source[index] !== "#") return index;
    let bracket = index + 1;
    while (/\s/.test(source[bracket] ?? "")) bracket += 1;
    if (source[bracket] !== "[") return index;
    let depth = 1;
    index = bracket + 1;
    while (index < source.length && depth > 0) {
      if (source[index] === "[") depth += 1;
      else if (source[index] === "]") depth -= 1;
      index += 1;
    }
    if (depth !== 0) return source.length;
  }
  return index;
}

function maskCfgTestItems(source) {
  const code = maskRustNonCode(source);
  const ranges = [];
  for (const attribute of cfgAttributes(source)) {
    if (!cfgPredicateImpliesTest(attribute.predicate)) continue;
    const itemStart = skipRustAttributes(code, attribute.end);
    const declaration = code.slice(itemStart).match(
      /^(?:(?:pub(?:\s*\([^)]*\))?|async|const|unsafe|extern(?:\s+"[^"]*")?)\s+)*(?:fn|mod|impl)\b/,
    );
    if (!declaration) continue;
    const bodyStart = itemBodyStart(code, itemStart);
    if (bodyStart === undefined) continue;
    ranges.push([attribute.start, matchingBraceEnd(code, bodyStart)]);
  }
  if (ranges.length === 0) return code;
  const characters = code.split("");
  for (const [start, end] of ranges) {
    for (let index = start; index < end; index += 1) {
      if (characters[index] !== "\n") characters[index] = " ";
    }
  }
  return characters.join("");
}

function rustUseDeclarations(source) {
  const declarations = [];
  for (const declaration of source.matchAll(/(?<![A-Za-z0-9_#'])use\b/g)) {
    const end = source.indexOf(";", declaration.index + declaration[0].length);
    if (end < 0) fail("unterminated Rust use declaration");
    declarations.push({
      start: declaration.index,
      end: end + 1,
      path: source.slice(declaration.index + declaration[0].length, end).trim(),
    });
  }
  return declarations;
}

function rustUseAliases(source) {
  const aliases = [];
  for (const declaration of rustUseDeclarations(source)) {
    const match = declaration.path.match(
      /^([^{}]+?)\s+as\s+([A-Za-z][A-Za-z0-9_]*)$/s,
    );
    if (match) aliases.push({ path: match[1].trim(), alias: match[2] });
  }
  return aliases;
}

function maskRustUseDeclarations(source) {
  const characters = source.split("");
  for (const declaration of rustUseDeclarations(source)) {
    for (let index = declaration.start; index < declaration.end; index += 1) {
      if (characters[index] !== "\n") characters[index] = " ";
    }
  }
  return characters.join("");
}

function containingFunction(source, callIndex, relativePath) {
  const prefix = source.slice(0, callIndex);
  const functions = [
    ...prefix.matchAll(/\bfn\s+([A-Za-z][A-Za-z0-9_]*)\s*(?:<[^>{}]*>)?\s*\(/g),
  ];
  const declaration = functions.at(-1);
  if (!declaration) fail(`runtime mutation call in ${relativePath} is outside a function`);
  const bodyStart = source.indexOf("{", declaration.index + declaration[0].length);
  if (bodyStart < 0 || bodyStart >= callIndex) {
    fail(`runtime mutation call in ${relativePath} is outside a function body`);
  }
  return {
    name: declaration[1],
    declarationStart: declaration.index,
    bodyStart,
    bodyEnd: matchingBraceEnd(source, bodyStart),
  };
}

function receiverFamiliesForType(typeName) {
  const families = new Set();
  if (/\bRuntimeThreadHandle\b/.test(typeName)) families.add("thread");
  if (/\bRuntimeHost(?:Handle)?\b/.test(typeName)) families.add("host");
  if (/\bGoalRuntimeHandle\b/.test(typeName)) families.add("goal");
  if (/\bOperationHandle\b/.test(typeName)) families.add("operation");
  if (/\b(?:Tui)?InteractionBroker\b/.test(typeName)) families.add("interaction_broker");
  if (/\bTuiOperationController\b/.test(typeName)) families.add("operation_controller");
  if (/\bTuiActionDispatcher\b/.test(typeName)) families.add("action_dispatcher");
  if (/\bTuiAgentRuntime\b/.test(typeName)) families.add("agent_runtime");
  if (/\bRuntimePendingInteractionStore\b/.test(typeName)) {
    families.add("interaction_projection");
  }
  if (/\bTuiTurnControl\b/.test(typeName)) families.add("interaction_projection");
  if (/\bMentionSearchManager\b/.test(typeName)) families.add("mention_search");
  if (/\bAppState\b/.test(typeName)) families.add("app_state");
  if (/\bTaskRegistry\b/.test(typeName)) families.add("task_registry");
  return families;
}

function associatedTypeAliases(source) {
  const aliases = new Map();
  for (const match of source.matchAll(
    /\btype\s+([A-Za-z][A-Za-z0-9_]*)\s*=\s*([^;]+);/g,
  )) {
    aliases.set(match[1], receiverFamiliesForType(match[2]));
  }
  for (const { path: importedPath, alias } of rustUseAliases(source)) {
    aliases.set(alias, receiverFamiliesForType(importedPath));
  }
  return aliases;
}

function associatedQualifierFamilies(qualifier, aliases) {
  const direct = receiverFamiliesForType(qualifier);
  if (direct.size > 0) return direct;
  const normalized = qualifier.replace(/\s/g, "");
  const terminal = normalized.match(/(?:^|::)([A-Za-z][A-Za-z0-9_]*)$/)?.[1];
  return new Set(aliases.get(normalized) ?? aliases.get(terminal) ?? []);
}

function unqualifiedAuthorityApi(name) {
  return TUI_RUNTIME_ASSOCIATED_METHODS.get(name)?.find(
    ([family]) => family === "*",
  )?.[1];
}

function authorityFunctionImportAliases(source) {
  const aliases = new Map();
  for (const { path: importedPath, alias } of rustUseAliases(source)) {
    const importedName = importedPath
      .replace(/\s/g, "")
      .match(/(?:^|::)([A-Za-z][A-Za-z0-9_]*)$/)?.[1];
    const api = importedName ? unqualifiedAuthorityApi(importedName) : undefined;
    if (api) aliases.set(alias, api);
  }
  return aliases;
}

function userActionAliases(source) {
  const typeNames = new Set(["UserAction"]);
  const variantNames = new Set();
  for (const match of source.matchAll(
    /\btype\s+([A-Za-z][A-Za-z0-9_]*)\s*=\s*([^;]+);/g,
  )) {
    if (/\bUserAction\b/.test(match[2])) typeNames.add(match[1]);
  }
  for (const { path: importedPath, alias } of rustUseAliases(source)) {
    const segments = importedPath.replace(/\s/g, "").split("::");
    const actionIndex = segments.lastIndexOf("UserAction");
    if (actionIndex < 0) continue;
    if (actionIndex === segments.length - 1) typeNames.add(alias);
    else if (actionIndex === segments.length - 2) variantNames.add(alias);
  }
  return { typeNames, variantNames };
}

function typeNamesPattern(typeNames) {
  return [...typeNames]
    .sort((left, right) => right.length - left.length)
    .map((name) => name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join("|");
}

function typeIsUserActionValue(typeName, aliases) {
  const normalized = typeName
    .trim()
    .replace(/^(?:&\s*(?:'\w+\s*)?(?:mut\s+)?)+/, "")
    .replace(/\s/g, "");
  const typePattern = typeNamesPattern(aliases.typeNames);
  return new RegExp(
    `^(?:(?:r#)?[A-Za-z][A-Za-z0-9_]*::)*(?:${typePattern})$`,
  ).test(normalized);
}

function typeIsUserActionSender(typeName, aliases) {
  const sender = typeName.match(/\bSender\s*<([\s\S]*)>/);
  return sender ? typeIsUserActionValue(sender[1], aliases) : false;
}

function userActionExpressionState(expression, variables, aliases) {
  const normalized = stripBalancedOuterParentheses(expression.trim()).replace(/\s/g, "");
  if (aliases.variantNames.has(normalized)) return true;
  const typePattern = typeNamesPattern(aliases.typeNames);
  if (
    new RegExp(
      `(?:^|[^A-Za-z0-9_])(?:(?:r#)?[A-Za-z][A-Za-z0-9_]*::)*(?:${typePattern})::[A-Za-z][A-Za-z0-9_]*(?:\\{|\\(|$)`,
    ).test(normalized)
  ) {
    return true;
  }
  if (/^[A-Za-z][A-Za-z0-9_]*$/.test(normalized)) {
    return variables.get(normalized);
  }
  return undefined;
}

function simpleFlowVariables(source, functionInfo, beforeIndex, aliases) {
  const variables = new Map();
  const userActionSenders = new Set();
  const signature = source.slice(functionInfo.declarationStart, functionInfo.bodyStart);
  for (const parameter of signature.matchAll(
    /\b([A-Za-z][A-Za-z0-9_]*)\s*:\s*([^,)]+)/g,
  )) {
    if (typeIsUserActionValue(parameter[2], aliases)) variables.set(parameter[1], true);
    if (typeIsUserActionSender(parameter[2], aliases)) {
      userActionSenders.add(parameter[1]);
    }
  }

  const prefix = source.slice(functionInfo.bodyStart + 1, beforeIndex);
  const lets = [...prefix.matchAll(
    /\blet\s+(?:mut\s+)?([A-Za-z][A-Za-z0-9_]*)(?:\s*:\s*([^=;]+))?(?:\s*=\s*([^;]+))?\s*;/gs,
  )].map((match) => ({ kind: "let", match }));
  const assignments = [...prefix.matchAll(
    /(?<![A-Za-z0-9_!=<>])([A-Za-z][A-Za-z0-9_]*)\s*=\s*(?![=>])([^;]+);/gs,
  )]
    .filter(({ index }) => !lets.some(({ match }) => index >= match.index && index < match.index + match[0].length))
    .map((match) => ({ kind: "assign", match }));
  for (const event of [...lets, ...assignments].sort(
    (left, right) => left.match.index - right.match.index,
  )) {
    const name = event.match[1];
    if (event.kind === "let") {
      const [, , declaredType, initializer] = event.match;
      const isAction =
        (declaredType && typeIsUserActionValue(declaredType, aliases)) ||
        (initializer && userActionExpressionState(initializer, variables, aliases));
      if (isAction) variables.set(name, true);
      if (declaredType && typeIsUserActionSender(declaredType, aliases)) {
        userActionSenders.add(name);
      }
    } else if (userActionExpressionState(event.match[2], variables, aliases)) {
      variables.set(name, true);
    }
  }
  return { variables, userActionSenders };
}

function firstCallArgument(source, openParenthesis) {
  let parentheses = 1;
  let braces = 0;
  let brackets = 0;
  for (let index = openParenthesis + 1; index < source.length; index += 1) {
    const character = source[index];
    if (character === "(") parentheses += 1;
    else if (character === ")") {
      parentheses -= 1;
      if (parentheses === 0) return source.slice(openParenthesis + 1, index).trim();
    } else if (character === "{") braces += 1;
    else if (character === "}") braces -= 1;
    else if (character === "[") brackets += 1;
    else if (character === "]") brackets -= 1;
    else if (character === "," && parentheses === 1 && braces === 0 && brackets === 0) {
      return source.slice(openParenthesis + 1, index).trim();
    }
  }
  fail("unterminated Rust call argument");
}

function sourceFunctions(source) {
  const functions = [];
  for (const declaration of source.matchAll(
    /\bfn\s+([A-Za-z][A-Za-z0-9_]*)\s*(?:<[^>{}]*>)?\s*\(/g,
  )) {
    const bodyStart = itemBodyStart(source, declaration.index);
    if (bodyStart === undefined) continue;
    functions.push({
      name: declaration[1],
      declarationStart: declaration.index,
      bodyStart,
      bodyEnd: matchingBraceEnd(
        source,
        bodyStart,
        `function ${declaration[1]} at line ${source.slice(0, declaration.index).split("\n").length}`,
      ),
    });
  }
  return functions;
}

function authorityApiForExpression(expression, aliases) {
  const normalized = stripBalancedOuterParentheses(expression.trim()).replace(/\s/g, "");
  if (/^[A-Za-z][A-Za-z0-9_]*$/.test(normalized)) return aliases.get(normalized);
  const name = normalized.match(/(?:^|::)([A-Za-z][A-Za-z0-9_]*)$/)?.[1];
  return name ? unqualifiedAuthorityApi(name) : undefined;
}

function scanAuthorityFunctionAliasFlow(source, importedAliases, recordSite, relativePath) {
  let functions;
  try {
    functions = sourceFunctions(source);
  } catch (error) {
    fail(`${relativePath}: ${error.message}`);
  }
  for (const functionInfo of functions) {
    const aliases = new Map(importedAliases);
    const body = source.slice(functionInfo.bodyStart + 1, functionInfo.bodyEnd - 1);
    for (const statement of body.matchAll(/[^;]*;/gs)) {
      for (const call of statement[0].matchAll(
        /\b([A-Za-z][A-Za-z0-9_]*)\s*\(/g,
      )) {
        const api = aliases.get(call[1]);
        if (api) recordSite(functionInfo.name, api);
      }
      const binding = statement[0].match(
        /\blet\s+(?:mut\s+)?([A-Za-z][A-Za-z0-9_]*)(?:\s*:\s*[^=;]+)?\s*=\s*([^;]+);/s,
      );
      const assignment = binding ?? statement[0].match(
        /(?<![A-Za-z0-9_!=<>])([A-Za-z][A-Za-z0-9_]*)\s*=\s*(?![=>])([^;]+);/s,
      );
      if (!assignment) continue;
      const api = authorityApiForExpression(assignment[2], aliases);
      if (!api) continue;
      if (/^[A-Za-z][A-Za-z0-9_]*$/.test(assignment[2].trim())) {
        recordSite(functionInfo.name, api);
      }
      aliases.set(assignment[1], api);
    }
  }
}

function possibleUserActionSender(receiver, userActionSenders) {
  const normalized = normalizeReceiverExpression(receiver).replace(/\s/g, "");
  if (userActionSenders.has(normalized)) return true;
  if (/^(?:self\.)?action_tx$/.test(normalized)) return true;
  const root = normalized.match(/^([A-Za-z][A-Za-z0-9_]*)\./)?.[1];
  return root ? userActionSenders.has(root) : false;
}

function rootReceiverFamilies(expression) {
  const normalized = expression.replace(/\s/g, "");
  const families = new Set();
  if (["runtime_thread", "thread", "resumed", "previous", "self.thread"].includes(normalized)) {
    families.add("thread");
  }
  if (["host", "runtime", "runtime_host", "self.host"].includes(normalized)) {
    families.add("host");
  }
  if (["runtime", "goal_runtime"].includes(normalized)) families.add("goal");
  if (["broker", "self.broker", "controller.broker()"].includes(normalized)) {
    families.add("interaction_broker");
  }
  if (/(?:^|\.)broker\(\)$/.test(normalized)) families.add("interaction_broker");
  if (["controller", "self.controller"].includes(normalized)) {
    families.add("operation_controller");
  }
  if (["dispatcher", "self.dispatcher"].includes(normalized)) {
    families.add("action_dispatcher");
  }
  if (["agent_runtime", "self.agent_runtime"].includes(normalized)) {
    families.add("agent_runtime");
  }
  if (normalized === "self.lock_state()") families.add("interaction_broker");
  if (normalized === "task_registry") families.add("task_registry");
  if (["approval_allowlist", "state.approval_allowlist"].includes(normalized)) {
    families.add("approval_allowlist");
  }
  return families;
}

function receiverExpressionBefore(source, methodDotIndex) {
  let index = methodDotIndex - 1;
  while (/\s/.test(source[index] ?? "")) index -= 1;
  const end = index + 1;
  let parentheses = 0;
  let brackets = 0;
  while (index >= 0) {
    const character = source[index];
    if (parentheses === 0 && brackets === 0 && /\s/.test(character)) {
      let left = index - 1;
      let right = index + 1;
      while (/\s/.test(source[left] ?? "")) left -= 1;
      while (/\s/.test(source[right] ?? "")) right += 1;
      if (source[left] !== "." && source[right] !== ".") break;
      index -= 1;
      continue;
    }
    if (character === ")") parentheses += 1;
    else if (character === "]") brackets += 1;
    else if (character === "(") {
      if (parentheses === 0) break;
      parentheses -= 1;
    } else if (character === "[") {
      if (brackets === 0) break;
      brackets -= 1;
    } else if (
      parentheses === 0 &&
      brackets === 0 &&
      /[;={},!|+\-/%?]/.test(character)
    ) {
      break;
    }
    index -= 1;
  }
  return source.slice(index + 1, end).trim();
}

function stripBalancedOuterParentheses(expression) {
  if (!expression.startsWith("(") || !expression.endsWith(")")) return expression;
  let depth = 0;
  for (let index = 0; index < expression.length; index += 1) {
    if (expression[index] === "(") depth += 1;
    else if (expression[index] === ")") depth -= 1;
    if (depth === 0 && index < expression.length - 1) return expression;
  }
  return depth === 0 ? expression.slice(1, -1).trim() : expression;
}

function normalizeReceiverExpression(rawExpression) {
  let expression = rawExpression.trim();
  while (true) {
    const before = expression;
    expression = stripBalancedOuterParentheses(expression);
    expression = expression.replace(/^(?:&\s*(?:mut\s+)?|\*\s*)+/, "").trim();
    expression = expression
      .replace(/\.\s*(?:clone|as_ref|as_mut)\s*\(\s*\)\s*$/, "")
      .trim();
    const staticClone = expression.match(/^(?:Arc|Rc|Clone)\s*::\s*clone\s*\((.*)\)$/s);
    if (staticClone) expression = staticClone[1].trim();
    if (expression === before) return expression;
  }
}

function enclosingImplFamilies(source, functionInfo) {
  const families = new Set();
  const prefix = source.slice(0, functionInfo.declarationStart);
  for (const declaration of prefix.matchAll(/\bimpl\b/g)) {
    const bodyStart = source.indexOf("{", declaration.index + declaration[0].length);
    if (bodyStart < 0 || bodyStart >= functionInfo.declarationStart) continue;
    const bodyEnd = matchingBraceEnd(source, bodyStart);
    if (bodyEnd <= functionInfo.declarationStart) continue;
    const header = source.slice(declaration.index, bodyStart);
    for (const family of receiverFamiliesForType(header)) families.add(family);
  }
  return families;
}

function receiverFamiliesAtCall(source, callIndex, receiver, relativePath) {
  const functionInfo = containingFunction(source, callIndex, relativePath);
  const scopes = [new Map()];
  scopes[0].set("self", enclosingImplFamilies(source, functionInfo));
  const signature = source.slice(functionInfo.declarationStart, functionInfo.bodyStart);
  for (const parameter of signature.matchAll(/\b([A-Za-z][A-Za-z0-9_]*)\s*:\s*([^,)]+)/g)) {
    scopes[0].set(parameter[1], receiverFamiliesForType(parameter[2]));
  }

  const lookup = (name) => {
    for (let index = scopes.length - 1; index >= 0; index -= 1) {
      if (scopes[index].has(name)) return scopes[index].get(name);
    }
    return undefined;
  };
  const resolve = (rawExpression) => {
    const expression = normalizeReceiverExpression(rawExpression);
    if (/^[A-Za-z][A-Za-z0-9_]*$/.test(expression)) {
      const alias = lookup(expression);
      if (alias !== undefined) return new Set(alias);
    }
    const roots = rootReceiverFamilies(expression);
    if (roots.size > 0) return roots;
    const rootAlias = expression.match(/^([A-Za-z][A-Za-z0-9_]*)\s*\./);
    if (rootAlias) {
      const alias = lookup(rootAlias[1]);
      if (alias !== undefined) return new Set(alias);
    }
    return roots;
  };

  let index = functionInfo.bodyStart + 1;
  while (index < callIndex) {
    if (source[index] === "{") {
      scopes.push(new Map());
      index += 1;
      continue;
    }
    if (source[index] === "}") {
      if (scopes.length > 1) scopes.pop();
      index += 1;
      continue;
    }
    if (/\blet\b/.test(source.slice(Math.max(functionInfo.bodyStart, index - 1), index + 4))) {
      const binding = source
        .slice(index)
        .match(
          /^let\s+(?:mut\s+)?([A-Za-z][A-Za-z0-9_]*)(?:\s*:\s*([^=;]+))?\s*=\s*([^;]+);/s,
        );
      if (binding) {
        const fromExpression = resolve(binding[3]);
        const fromType = binding[2] ? receiverFamiliesForType(binding[2]) : new Set();
        const fromName = rootReceiverFamilies(binding[1]);
        const families =
          [fromExpression, fromType, fromName].find((candidate) => candidate.size > 0) ?? new Set();
        scopes.at(-1).set(binding[1], families);
        index += binding[0].length;
        continue;
      }
    }
    index += 1;
  }
  return {
    families: resolve(receiver),
    functionName: functionInfo.name,
    functionSha256: canonicalSourceSha256(
      source.slice(functionInfo.declarationStart, functionInfo.bodyEnd),
    ),
  };
}

function isFunctionDeclaration(source, callIndex) {
  return /\bfn\s*$/.test(source.slice(Math.max(0, callIndex - 32), callIndex));
}

function scanTuiMutationSurface({ repoRoot, sourceOverrides, sourcePaths: suppliedSourcePaths }) {
  const sites = new Map();
  const harmlessSameNameSites = new Map();
  const harmlessSameNameFunctionHashes = new Map();
  const harmlessAssociatedSites = new Map();
  const harmlessAssociatedFunctionHashes = new Map();
  const unresolvedUserActionSendSites = new Map();
  const unresolvedUserActionSendFunctionHashes = new Map();
  const sourcePaths = suppliedSourcePaths ?? tuiRustSourcePaths(repoRoot);
  const cfgTestModules = cfgTestExternalModulePaths(repoRoot, sourcePaths, sourceOverrides);
  for (const relativePath of sourcePaths) {
    if (cfgTestModules.has(relativePath)) continue;
    const source = maskCfgTestItems(
      readRepoSource(repoRoot, relativePath, sourceOverrides),
    );
    const typeAliases = associatedTypeAliases(source);
    const associatedSource = maskRustUseDeclarations(source);
    const recordSite = (functionName, api) => {
      const key = `${relativePath}:${functionName}:${api}`;
      sites.set(key, (sites.get(key) ?? 0) + 1);
    };
    scanAuthorityFunctionAliasFlow(
      source,
      authorityFunctionImportAliases(source),
      recordSite,
      relativePath,
    );
    for (const [api, patterns] of TUI_RUNTIME_MUTATION_APIS) {
      for (const pattern of patterns) {
        for (const match of source.matchAll(pattern)) {
          if (isFunctionDeclaration(source, match.index)) continue;
          recordSite(containingFunction(source, match.index, relativePath).name, api);
        }
      }
    }
    const associatedMethodNames = [...TUI_RUNTIME_ASSOCIATED_METHODS.keys()]
      .sort((left, right) => right.length - left.length)
      .join("|");
    const associatedItem = new RegExp(
      `(?<![A-Za-z0-9_])((?:[A-Za-z][A-Za-z0-9_]*\\s*::\\s*)+|<[^;{}()]+>\\s*::\\s*)(${associatedMethodNames})\\b`,
      "g",
    );
    for (const match of associatedSource.matchAll(associatedItem)) {
      const qualifier = match[1].replace(/\s*::\s*$/, "").trim();
      const families = associatedQualifierFamilies(qualifier, typeAliases);
      const functionInfo = containingFunction(source, match.index, relativePath);
      let classified = false;
      for (const [family, api] of TUI_RUNTIME_ASSOCIATED_METHODS.get(match[2])) {
        if (family === "*" || families.has(family)) {
          recordSite(functionInfo.name, api);
          classified = true;
        }
      }
      if (!classified) {
        const item = `${qualifier.replace(/\s/g, "")}::${match[2]}`;
        const key = `${relativePath}:${functionInfo.name}:${item}`;
        harmlessAssociatedSites.set(key, (harmlessAssociatedSites.get(key) ?? 0) + 1);
        harmlessAssociatedFunctionHashes.set(
          `${relativePath}:${functionInfo.name}`,
          canonicalSourceSha256(
            source.slice(functionInfo.declarationStart, functionInfo.bodyEnd),
          ),
        );
      }
    }
    const actionAliases = userActionAliases(source);
    for (const match of source.matchAll(/\.\s*send\s*\(/g)) {
      const receiver = receiverExpressionBefore(source, match.index);
      const functionInfo = containingFunction(source, match.index, relativePath);
      const flow = simpleFlowVariables(source, functionInfo, match.index, actionAliases);
      const openParenthesis = match.index + match[0].lastIndexOf("(");
      const argument = firstCallArgument(source, openParenthesis);
      if (userActionExpressionState(argument, flow.variables, actionAliases)) {
        recordSite(functionInfo.name, "user_action.route");
      } else if (possibleUserActionSender(receiver, flow.userActionSenders)) {
        const key = `${relativePath}:${functionInfo.name}:${receiver.replace(/\s/g, "")}.send`;
        unresolvedUserActionSendSites.set(
          key,
          (unresolvedUserActionSendSites.get(key) ?? 0) + 1,
        );
        unresolvedUserActionSendFunctionHashes.set(
          `${relativePath}:${functionInfo.name}`,
          canonicalSourceSha256(
            source.slice(functionInfo.declarationStart, functionInfo.bodyEnd),
          ),
        );
      }
    }
    const methodNames = [...TUI_RUNTIME_RECEIVER_METHODS.keys()]
      .sort((left, right) => right.length - left.length)
      .join("|");
    const receiverCall = new RegExp(`\\.\\s*(${methodNames})\\s*\\(`, "g");
    for (const match of source.matchAll(receiverCall)) {
      const receiver = receiverExpressionBefore(source, match.index);
      const { families, functionName, functionSha256 } = receiverFamiliesAtCall(
        source,
        match.index,
        receiver,
        relativePath,
      );
      let classified = false;
      for (const [family, api] of TUI_RUNTIME_RECEIVER_METHODS.get(match[1])) {
        if (families.has(family)) {
          recordSite(functionName, api);
          classified = true;
        }
      }
      const callText = source.slice(match.index, source.indexOf(")", match.index) + 1);
      if (match[1] === "interrupt" && /\.\s*interrupt\s*\(\s*\)/.test(callText)) {
        classified = true;
      }
      if (!classified) {
        const receiverMethod = `${receiver.replace(/\s/g, "")}.${match[1]}`;
        const key = `${relativePath}:${functionName}:${receiverMethod}`;
        harmlessSameNameSites.set(key, (harmlessSameNameSites.get(key) ?? 0) + 1);
        harmlessSameNameFunctionHashes.set(
          `${relativePath}:${functionName}`,
          functionSha256,
        );
      }
    }
  }
  return {
    sites,
    harmlessSameNameSites,
    harmlessSameNameFunctionHashes,
    harmlessAssociatedSites,
    harmlessAssociatedFunctionHashes,
    unresolvedUserActionSendSites,
    unresolvedUserActionSendFunctionHashes,
  };
}

export function scanTuiMutationEntrypoints(options) {
  return scanTuiMutationSurface(options).sites;
}

export function scanTuiHarmlessSameNameEntrypoints(options) {
  return scanTuiMutationSurface(options).harmlessSameNameSites;
}

export function scanTuiHarmlessSameNameFunctionHashes(options) {
  return scanTuiMutationSurface(options).harmlessSameNameFunctionHashes;
}

export function scanTuiHarmlessAssociatedFunctionItems(options) {
  return scanTuiMutationSurface(options).harmlessAssociatedSites;
}

export function scanTuiHarmlessAssociatedFunctionHashes(options) {
  return scanTuiMutationSurface(options).harmlessAssociatedFunctionHashes;
}

function validateTuiMutationScan(repoRoot, sourceOverrides) {
  const {
    sites: actual,
    harmlessSameNameSites,
    harmlessSameNameFunctionHashes,
    harmlessAssociatedSites,
    harmlessAssociatedFunctionHashes,
    unresolvedUserActionSendSites,
    unresolvedUserActionSendFunctionHashes,
  } = scanTuiMutationSurface({ repoRoot, sourceOverrides });
  for (const [site, count] of actual) {
    if (!BASELINE_DIRECT_TUI_MUTATION_SITES.has(site)) {
      fail(`unlisted mutation-capable TUI entrypoint ${site.split(":").at(-2)}`);
    }
    const expected = BASELINE_DIRECT_TUI_MUTATION_SITES.get(site);
    const maximum = RETIRABLE_DIRECT_TUI_MUTATION_SITE_MAX_COUNTS.get(site) ?? expected;
    if (
      count > maximum ||
      (!RETIRABLE_DIRECT_TUI_MUTATION_SITE_MAX_COUNTS.has(site) && count !== expected)
    ) {
      fail(`TUI mutation call count drifted for ${site}`);
    }
  }
  for (const [site, count] of BASELINE_DIRECT_TUI_MUTATION_SITES) {
    if (RETIRABLE_DIRECT_TUI_MUTATION_SITE_MAX_COUNTS.has(site)) {
      const actualCount = actual.get(site) ?? 0;
      if (actualCount > RETIRABLE_DIRECT_TUI_MUTATION_SITE_MAX_COUNTS.get(site)) {
        fail(`TUI mutation call count drifted for ${site}`);
      }
    } else if (actual.get(site) !== count) {
      fail(`missing mutation-capable TUI entrypoint ${site}`);
    }
  }
  for (const [site, count] of harmlessSameNameSites) {
    if (!BASELINE_HARMLESS_SAME_NAME_METHOD_SITES.has(site)) {
      fail(`unclassified same-name TUI method ${site.split(":").at(-2)}`);
    }
    if (BASELINE_HARMLESS_SAME_NAME_METHOD_SITES.get(site) !== count) {
      fail(`harmless same-name TUI method count drifted for ${site}`);
    }
  }
  for (const [site, count] of BASELINE_HARMLESS_SAME_NAME_METHOD_SITES) {
    if (harmlessSameNameSites.get(site) !== count) {
      fail(`missing harmless same-name TUI method classification ${site}`);
    }
  }
  for (const [site, count] of harmlessAssociatedSites) {
    if (!BASELINE_HARMLESS_ASSOCIATED_FUNCTION_ITEM_SITES.has(site)) {
      fail(`unclassified associated TUI function item ${site.split(":", 2)[1]}`);
    }
    if (BASELINE_HARMLESS_ASSOCIATED_FUNCTION_ITEM_SITES.get(site) !== count) {
      fail(`harmless associated TUI function item count drifted for ${site}`);
    }
  }
  for (const [site, count] of BASELINE_HARMLESS_ASSOCIATED_FUNCTION_ITEM_SITES) {
    if (harmlessAssociatedSites.get(site) !== count) {
      fail(`missing harmless associated TUI function item classification ${site}`);
    }
  }
  for (const [functionSite, hash] of harmlessAssociatedFunctionHashes) {
    if (BASELINE_HARMLESS_ASSOCIATED_FUNCTION_SHA256.get(functionSite) !== hash) {
      fail(`harmless associated TUI function drifted for ${functionSite}`);
    }
  }
  for (const functionSite of BASELINE_HARMLESS_ASSOCIATED_FUNCTION_SHA256.keys()) {
    if (!harmlessAssociatedFunctionHashes.has(functionSite)) {
      fail(`missing harmless associated TUI function classification ${functionSite}`);
    }
  }
  for (const [site, count] of unresolvedUserActionSendSites) {
    if (!BASELINE_UNRESOLVED_USER_ACTION_SEND_SITES.has(site)) {
      fail(`unresolved possible UserAction send ${site.split(":").at(-2)}`);
    }
    if (BASELINE_UNRESOLVED_USER_ACTION_SEND_SITES.get(site) !== count) {
      fail(`unresolved possible UserAction send count drifted for ${site}`);
    }
  }
  for (const [site, count] of BASELINE_UNRESOLVED_USER_ACTION_SEND_SITES) {
    if (unresolvedUserActionSendSites.get(site) !== count) {
      fail(`missing unresolved possible UserAction send classification ${site}`);
    }
  }
  for (const [functionSite, hash] of unresolvedUserActionSendFunctionHashes) {
    if (BASELINE_UNRESOLVED_USER_ACTION_SEND_FUNCTION_SHA256.get(functionSite) !== hash) {
      fail(`unresolved possible UserAction send function drifted for ${functionSite}`);
    }
  }
  for (const functionSite of BASELINE_UNRESOLVED_USER_ACTION_SEND_FUNCTION_SHA256.keys()) {
    if (!unresolvedUserActionSendFunctionHashes.has(functionSite)) {
      fail(`missing unresolved possible UserAction send function classification ${functionSite}`);
    }
  }
}

export function validateCurrentInventories(manifest, { repoRoot, sourceOverrides }) {
  validateNoUnstableSurfaceReferences(repoRoot, sourceOverrides);

  assertExactArray(
    parseSurfaceFacadeExports(
      readRepoSource(repoRoot, "crates/orca-runtime/src/lib.rs", sourceOverrides),
    ),
    RUNTIME_SURFACE_MODULES.flatMap(
      (moduleName) => manifest.runtime_surface_public_exports[moduleName],
    ).sort(),
    "current surface facade exports",
  );

  const runtimeSurfaceModulePath = "crates/orca-runtime/src/runtime_surface/mod.rs";
  const runtimeSurfaceExports = parseRuntimeSurfacePublicExports(
    readRepoSource(repoRoot, runtimeSurfaceModulePath, sourceOverrides),
  );
  assertExactArray(
    Object.keys(runtimeSurfaceExports),
    RUNTIME_SURFACE_MODULES,
    "current runtime-surface public export modules",
  );
  for (const moduleName of RUNTIME_SURFACE_MODULES) {
    assertExactArray(
      runtimeSurfaceExports[moduleName],
      manifest.runtime_surface_public_exports[moduleName],
      `current runtime-surface ${moduleName} public exports`,
    );
    assertNoProductionRuntimeSurfaceSiblingGlobs(
      readRepoSource(
        repoRoot,
        `crates/orca-runtime/src/runtime_surface/${moduleName}.rs`,
        sourceOverrides,
      ),
      moduleName,
    );
  }

  const eventSchemaPath = checkedRepoFile(
    repoRoot,
    "crates/orca-core/src/event_schema.rs",
    "EventType source",
  );
  const eventSchema = readRepoSource(
    repoRoot,
    relativeRepoPath(repoRoot, eventSchemaPath),
    sourceOverrides,
  );
  const eventTypes = parseRustEnum(eventSchema, "pub enum EventType {");
  const sourceFacts = manifest.source_facts.map((row) => row[0]);
  assertExactArray(sourceFacts, eventTypes, "source_facts current EventType");
  manifest.source_facts.forEach((row) => {
    const line = eventSchema.split(/\r?\n/)[row[1] - 1] ?? "";
    if (!line.includes(row[0])) fail(`${row[0]} source line ${row[1]} has drifted`);
  });

  const userActionPath = checkedRepoFile(repoRoot, "crates/orca-tui/src/types.rs", "UserAction source");
  const userActions = parseRustEnum(
    readRepoSource(repoRoot, relativeRepoPath(repoRoot, userActionPath), sourceOverrides),
    "pub enum UserAction {",
  );
  assertExactArray(
    manifest.closed_inventory.current_tui_user_actions,
    userActions,
    "current_tui_user_actions current UserAction",
  );
  const currentRows = manifest.tui_actions.filter((row) => row[1] === "current");
  const futureRows = manifest.tui_actions.filter((row) => row[1] === "required_addition");
  assertExactArray(
    currentRows.map((row) => row[0]),
    manifest.closed_inventory.current_tui_user_actions,
    "current tui_actions",
  );
  assertExactArray(
    futureRows.map((row) => row[0]),
    manifest.closed_inventory.required_tui_user_action_additions,
    "future tui_actions",
  );
  assertExactArray(
    manifest.tui_entrypoints.map((row) => row[0]),
    [...TUI_ENTRYPOINT_ANCHORS.keys()],
    "tui_entrypoints",
  );
  for (const row of manifest.tui_entrypoints) {
    for (const reference of requireArray(row[1], `${row[0]} sources`)) {
      const source = validateSourceReference(
        repoRoot,
        reference,
        `${row[0]} source`,
        sourceOverrides,
      );
      if (!source.relativePath.startsWith("crates/orca-tui/src/")) {
        fail(`${row[0]} source is outside crates/orca-tui/src/: ${reference}`);
      }
      if (!tuiEntrypointAnchor(row[0], source.relativePath).test(source.source)) {
        fail(`${row[0]} source does not contain its reviewed entrypoint anchor: ${reference}`);
      }
    }
    if (!row[4] || !row[6] || !row[7]) fail(`${row[0]} has an incomplete target boundary`);
  }
  for (const row of manifest.tui_actions) {
    for (const reference of requireArray(row[2], `${row[0]} sources`)) {
      const source = validateSourceReference(
        repoRoot,
        reference,
        `${row[0]} source`,
        sourceOverrides,
      );
      const anchor = tuiActionSourceAnchor(row[0], source.relativePath);
      if (anchor && !anchor.test(source.source)) {
        fail(`${row[0]} source does not contain its reviewed action anchor: ${reference}`);
      }
    }
  }
  for (const [id, sourcePath, sourceLine] of manifest.non_event_sources) {
    const absolute = checkedRepoFile(repoRoot, sourcePath, `${id} source`);
    const lineCount = readFileSync(absolute, "utf8").split(/\r?\n/).length;
    if (!Number.isInteger(sourceLine) || sourceLine < 1 || sourceLine > lineCount) {
      fail(`${id} source line ${sourceLine} has drifted`);
    }
  }
  validateTuiMutationScan(repoRoot, sourceOverrides);
}

export function validateRuntimeSurfaceContract({
  repoRoot,
  manifestPath = path.join(repoRoot, DEFAULT_MANIFEST),
  digestPath = path.join(repoRoot, DEFAULT_DIGEST),
  emitSuccess = true,
}) {
  const absoluteManifestPath = path.resolve(manifestPath);
  const manifest = parseManifestText(readFileSync(absoluteManifestPath, "utf8"));
  validateManifestStructure(manifest);
  validateArtifactBundle(manifest, { repoRoot });
  validateArtifactDigest(JSON.parse(readFileSync(path.resolve(digestPath), "utf8")), {
    repoRoot,
  });
  validateCurrentInventories(manifest, { repoRoot });
  if (emitSuccess) console.log("runtime surface contract validated");
  return manifest;
}

function parseArguments(argv) {
  let repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  let manifestPath;
  let digestPath;
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--repo-root") repoRoot = path.resolve(argv[++index]);
    else if (argument === "--manifest") manifestPath = path.resolve(argv[++index]);
    else if (argument === "--digest") digestPath = path.resolve(argv[++index]);
    else fail(`unknown argument ${argument}`);
  }
  return {
    repoRoot,
    manifestPath: manifestPath ?? path.join(repoRoot, DEFAULT_MANIFEST),
    digestPath: digestPath ?? path.join(repoRoot, DEFAULT_DIGEST),
  };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    validateRuntimeSurfaceContract(parseArguments(process.argv.slice(2)));
  } catch (error) {
    console.error(`runtime surface contract validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
