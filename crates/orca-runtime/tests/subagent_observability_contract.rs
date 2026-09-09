//! Architectural contracts for the parent/child observability path.
//!
//! These checks intentionally inspect the private integration points as source
//! text.  The actor loop and the hosted controller are private implementation
//! details, so an external test cannot exercise their negative paths without
//! manufacturing an invalid runtime authority.  Keeping the assertions here
//! makes the intended wiring explicit while the implementation is being
//! completed; each assertion should become a normal behavioural test once the
//! corresponding public surface exists.

const RUNTIME_HOST: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime_host.rs"));
const GENERATION_ACTOR: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_actor/thread_actor_generation.rs"
));
const CHILD_TYPES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/child_agent_types.rs"
));
const SYNC_SUBAGENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_subagent_call.rs"
));
const ASYNC_SUBAGENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/subagent_async_worker.rs"
));
const AGENT_CONTROLLER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/agent_controller.rs"
));
const WORKFLOW_RUNNER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/workflow/runner.rs"
));
const HOSTED_CONTROLLER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/hosted_controller.rs"
));
const HOSTED_CHILD: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/hosted_child.rs"
));
const HOSTED_SESSION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/hosted_session.rs"
));
const TUI_PROTOCOL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/protocol.rs"
));
const TUI_AGENT_WORKSPACE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/agent_workspace.rs"
));
const TUI_STATE_REDUCER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/state_reducer.rs"
));
const TUI_UI: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/ui.rs"
));
const SURFACE_IDENTITY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_surface/identity.rs"
));
const SURFACE_PROJECTION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_surface/projection.rs"
));
const SURFACE_REDUCER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_surface/reducer.rs"
));
const SURFACE_STORE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_surface/store.rs"
));

fn balanced_block<'a>(source: &'a str, marker: &str) -> &'a str {
    let marker_start = source
        .find(marker)
        .unwrap_or_else(|| panic!("source does not contain {marker:?}"));
    let open = source[marker_start..]
        .find('{')
        .map(|offset| marker_start + offset)
        .expect("marker should introduce a braced block");
    let mut depth = 0usize;
    for (offset, character) in source[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth
                    .checked_sub(1)
                    .expect("balanced source block underflow");
                if depth == 0 {
                    return &source[marker_start..open + offset + character.len_utf8()];
                }
            }
            _ => {}
        }
    }
    panic!("source block {marker:?} is not balanced");
}

#[test]
fn detached_relays_are_woken_in_idle_and_active_actor_states() {
    // A detached worker outlives the parent generation.  Polling only the
    // active branch leaves a durable relay stranded as soon as the parent
    // generation returns, which is exactly when an async child is most likely
    // to continue producing events.
    let poll_arm = "_ = tokio::time::sleep(SUBAGENT_RELAY_POLL_INTERVAL) =>";
    let poll_count = RUNTIME_HOST.matches(poll_arm).count();
    assert!(
        poll_count >= 2,
        "relay polling must be present in both idle and active select! branches; found {poll_count} arm(s)"
    );
}

#[test]
fn task_transcript_action_is_dispatched_instead_of_dropped() {
    let start = HOSTED_CONTROLLER
        .find("Ok(UserAction::ReadTaskTranscript(")
        .expect("hosted controller must handle ReadTaskTranscript");
    let remainder = &HOSTED_CONTROLLER[start..];
    let end = remainder
        .find("Ok(UserAction::ResolveBackgroundApproval")
        .expect("transcript action should be followed by the next action arm");
    let arm = &remainder[..end];

    assert!(
        !arm.contains("=> {}"),
        "ReadTaskTranscript must not be a silent no-op"
    );
    assert!(
        arm.contains("read_task_transcript")
            || arm.contains("HostedTaskAction::")
            || arm.contains("handle_hosted_task_action")
            || arm.contains("event_tx.send"),
        "transcript action must dispatch to a typed runtime/read-result path"
    );
}

#[test]
fn hosted_child_navigation_requires_a_surface_lifecycle_binding() {
    let focus = balanced_block(HOSTED_CHILD, "fn focus_child");
    let projection = balanced_block(HOSTED_CHILD, "fn project_hosted_child_attached");
    let resolver = balanced_block(HOSTED_CHILD, "fn resolve_child_binding");
    let surface_binding = balanced_block(HOSTED_CHILD, "fn child_binding");

    assert!(focus.contains("project_hosted_child_attached"));
    assert!(projection.contains("TuiEvent::ChildProjectionReset"));
    assert!(resolver.contains("child_binding"));
    assert!(surface_binding.contains("SurfaceSubagentStatus::Running"));
    assert!(!HOSTED_CHILD.contains("registry_child_binding"));
    assert!(!HOSTED_CHILD.contains("agent_registry_snapshot"));
}

#[test]
fn every_child_runtime_constructor_carries_permission_identity() {
    let context = balanced_block(CHILD_TYPES, "pub(crate) struct ChildAgentRuntimeContext");
    assert!(
        context.contains("permission"),
        "ChildAgentRuntimeContext needs an explicit permission handler/context field"
    );
    assert!(
        context.contains("root_task_id") || context.contains("parent_task_id"),
        "child permission scope must retain parent/task identity"
    );

    for (name, source) in [
        ("synchronous subagent", SYNC_SUBAGENT),
        ("asynchronous subagent", ASYNC_SUBAGENT),
        ("workflow child", WORKFLOW_RUNNER),
    ] {
        let mut offset = 0usize;
        let mut found = false;
        while let Some(relative) = source[offset..].find("ChildAgentRuntimeContext {") {
            found = true;
            let start = offset + relative;
            let block = balanced_block(&source[start..], "ChildAgentRuntimeContext");
            assert!(
                block.contains("permission"),
                "{name} ChildAgentRuntimeContext construction must pass the child permission scope"
            );
            offset = start + "ChildAgentRuntimeContext".len();
        }
        assert!(
            found,
            "expected a production {name} ChildAgentRuntimeContext"
        );
    }
}

#[test]
fn synchronous_child_activity_has_one_surface_delivery_boundary() {
    // Hosted execution uses the typed surface sink. The standalone CLI keeps
    // its established observer path when no hosted surface ingress exists.
    let sink_selection = balanced_block(SYNC_SUBAGENT, "let (activity_owner, activity_sink)");
    assert!(
        !sink_selection.contains("TaskRegistryActivitySink"),
        "sync child activity must not fall back to a registry-only mirror"
    );
    assert!(
        sink_selection.contains("RuntimeSubagentActivitySink")
            && sink_selection.contains("Some(activity_ingress)"),
        "hosted sync children must use the typed surface sink when available"
    );
}

#[test]
fn surface_backed_agent_delivery_is_exclusive_from_the_registry() {
    assert!(AGENT_CONTROLLER.contains("enum AgentActivityDelivery"));
    assert!(AGENT_CONTROLLER.contains("Surface(AgentSurfacePublisher)"));
    assert!(AGENT_CONTROLLER.contains("Registry(Arc<crate::agent_registry::AgentRegistry>)"));
    let publisher = balanced_block(AGENT_CONTROLLER, "impl AgentEventPublisher");
    assert!(publisher.contains("let delivery = match surface_activity"));
    assert!(publisher.contains("AgentActivityDelivery::Surface"));
    assert!(publisher.contains("AgentActivityDelivery::Registry"));
}

#[test]
fn hosted_tui_subagent_views_have_no_registry_input() {
    assert!(!HOSTED_CONTROLLER.contains("agent_registry_snapshot"));
    assert!(!HOSTED_CONTROLLER.contains("AgentRegistryUpdated"));
    assert!(!HOSTED_CHILD.contains("agent_registry_snapshot"));
    assert!(!TUI_PROTOCOL.contains("AgentRegistryUpdated"));
    assert!(!TUI_AGENT_WORKSPACE.contains("AgentSummary"));
    assert!(!TUI_AGENT_WORKSPACE.contains("RegistryAgent"));
    assert!(!TUI_UI.contains("agent_registry_activity_lines"));
}

#[test]
fn subagent_runtime_events_are_not_a_parallel_tui_projection() {
    let mapper = balanced_block(HOSTED_SESSION, "fn runtime_event_to_tui");
    let reducer = balanced_block(TUI_STATE_REDUCER, "pub fn update");
    let projection = balanced_block(
        TUI_STATE_REDUCER,
        "pub(crate) fn apply_surface_projection_state",
    );

    assert!(mapper.contains(
        "EventType::SubagentStarted | EventType::SubagentProgress | EventType::SubagentCompleted"
    ));
    assert!(reducer.contains("TuiEvent::SurfaceProjectionSynced(projection)"));
    assert!(reducer.contains("self.apply_surface_projection_state(*projection)"));
    assert!(projection.contains("self.apply_workflow_tasks_update"));
}

#[test]
fn surface_task_projection_preserves_the_registry_parent_identity() {
    let function = balanced_block(GENERATION_ACTOR, "fn commit_subagent_activity_inner");
    assert!(
        function.contains("resolve_subagent_parent_task_id")
            && function.contains("parent_task_id,"),
        "the first child task projection must derive parent_task_id from the registry and tolerate publication order"
    );
}

#[test]
fn durable_commit_retry_checks_the_event_digest_before_acknowledging_id() {
    let function = balanced_block(GENERATION_ACTOR, "fn commit_subagent_activity_inner");
    let lookup_start = function
        .find("lookup_commit(&event.surface_commit_id)")
        .expect("subagent activity commit should consult the durable retry index");
    let lookup_block = &function[lookup_start..];
    assert!(
        lookup_block.contains("stored_batch_digest")
            && lookup_block.contains("batch_digest")
            && lookup_block.contains("event.digest"),
        "an existing commit id is not sufficient: durable retry acknowledgement must bind the stored batch digest to this event"
    );
}

#[test]
fn detached_owner_identity_and_source_cursor_are_durable_surface_types() {
    assert!(SURFACE_IDENTITY.contains("pub struct SurfaceTaskOwnerRef"));
    assert!(SURFACE_IDENTITY.contains("SurfaceTaskAttemptId"));
    assert!(SURFACE_PROJECTION.contains("enum SurfaceSubagentOwner"));
    assert!(SURFACE_PROJECTION.contains("DetachedTask"));
    assert!(SURFACE_PROJECTION.contains("source_digest"));
    assert!(SURFACE_PROJECTION.contains("source_commit_id"));
    assert!(SURFACE_PROJECTION.contains("source_sequence"));
    assert!(SURFACE_PROJECTION.contains("occurred_at"));
    assert!(SURFACE_STORE.contains("Subagent(SubagentPatch)"));
}

#[test]
fn reducer_and_actor_authorize_detached_owner_without_generation_fence() {
    assert!(SURFACE_REDUCER.contains("SurfaceSubagentOwner::DetachedTask"));
    assert!(SURFACE_REDUCER.contains("source_sequence"));
    assert!(GENERATION_ACTOR.contains("commit_detached_subagent_activity"));
    assert!(GENERATION_ACTOR.contains("DetachedTask"));
}

#[test]
fn source_digest_is_indexed_for_cross_restart_commit_conflicts() {
    assert!(SURFACE_STORE.contains("lookup_subagent_source_digest"));
    assert!(GENERATION_ACTOR.contains("lookup_subagent_source_digest"));
    assert!(GENERATION_ACTOR.contains("event.digest"));
}

#[test]
fn quarantined_relay_health_is_idempotent_after_first_surface_commit() {
    let function = balanced_block(GENERATION_ACTOR, "fn surface_subagent_relay_corruption");
    assert!(function.contains("already_surfaced"));
    assert!(function.contains("return Ok(())"));
    assert!(
        function.contains("session_health") && function.contains("HealthIssueId"),
        "relay corruption must be represented by a typed durable health issue"
    );
}

#[test]
fn relay_lifecycle_failures_have_bounded_retry_and_durable_health() {
    let poll_failure = balanced_block(GENERATION_ACTOR, "fn record_subagent_relay_poll_failure");
    let lifecycle_failure = balanced_block(
        GENERATION_ACTOR,
        "fn surface_subagent_relay_lifecycle_failure",
    );

    assert!(poll_failure.contains("SUBAGENT_RELAY_FAILURE_POLL_LIMIT"));
    assert!(poll_failure.contains("record_subagent_relay_failure"));
    assert!(
        lifecycle_failure.contains("HealthIssueId::Projection")
            && lifecycle_failure.contains("relay_lifecycle_surface_events"),
        "relay lifecycle failures must remain visible in durable session health"
    );
    assert!(
        RUNTIME_HOST
            .matches("record_subagent_relay_poll_failure")
            .count()
            >= 3,
        "active and idle actor polling paths must share bounded failure accounting"
    );
    assert!(
        !RUNTIME_HOST.contains(
            "io::ErrorKind::NotFound | io::ErrorKind::Interrupted | io::ErrorKind::NotConnected"
        ),
        "NotFound and NotConnected must not be grouped with retry-only relay outcomes"
    );
}

#[test]
fn async_launch_commits_started_before_worker_spawn_and_reuses_turn_identity() {
    let launch = balanced_block(ASYNC_SUBAGENT, "pub(crate) fn launch_async_subagent");
    let started = launch
        .find("parent_activity.publish_payload(SubagentActivityPayload::Started")
        .expect("actor launch must commit Started before spawning the worker");
    let spawned = launch
        .find("spawn_async_subagent_worker(AsyncSubagentWorkerSpawnContext")
        .expect("actor launch must spawn the worker after precommit");
    assert!(started < spawned);
    assert!(launch[started..spawned].contains("mark_worker_spawned"));
    assert!(launch.contains("activity_start_precommitted: true"));
    assert!(launch.contains("child_turn_id: &child_turn_id"));
    assert!(ASYNC_SUBAGENT.contains("--child-turn-id"));
    assert!(ASYNC_SUBAGENT.contains("--activity-start-precommitted"));
}

#[test]
fn async_launch_failures_terminalize_continuation_before_surface_failure() {
    let launch = balanced_block(ASYNC_SUBAGENT, "pub(crate) fn launch_async_subagent");
    let helper = balanced_block(ASYNC_SUBAGENT, "fn finish_async_launch_failure");
    let continuation = helper
        .find("coordinator.commit_prepared_terminal(")
        .expect("launch failure helper must commit the continuation terminal");
    let surface = helper
        .find("activity.publish_payload(SubagentActivityPayload::Completed")
        .expect("launch failure helper must publish a failed surface terminal");
    assert!(continuation < surface);
    assert!(launch.matches("finish_async_launch_failure(").count() >= 3);
    let started_failure = launch
        .find("failed to commit async subagent Started activity")
        .expect("Started failure path must be explicit");
    let started_failure_block = &launch[started_failure..];
    assert!(
        !started_failure_block.contains("finish_async_launch_failure(")
            || started_failure_block
                .find("finish_async_launch_failure(")
                .is_some_and(|offset| {
                    started_failure_block[..offset].contains("The surface sequence starts at one")
                }),
        "a rejected Started event must not fabricate a sequence-two surface terminal"
    );
}

#[test]
fn async_surface_terminal_follows_schema_and_continuation_commit() {
    // Windows checkouts use CRLF, while this contract inspects Rust source
    // snippets. Normalize line endings so the architectural assertion is
    // platform-independent.
    let async_subagent = ASYNC_SUBAGENT.replace("\r\n", "\n");
    let worker = balanced_block(
        &async_subagent,
        "pub(crate) fn run_async_subagent_worker_with_executor",
    );
    let completed_task = worker
        .find("let completed_task =")
        .expect("async worker must settle its lifecycle task");
    let schema = worker
        .find("validate_subagent_output_schema")
        .expect("async worker must validate output schema");
    let panic_surface_terminal = worker
        .find("if projection.is_some()\n                && let Err(error) = activity.publish_payload(SubagentActivityPayload::Completed")
        .expect("async panic path must publish a surface terminal");
    assert!(
        panic_surface_terminal
            > worker
                .find("let projection = match commit_async_terminal(")
                .expect("async panic path must commit continuation terminal"),
        "panic surface terminal must follow continuation terminal"
    );
    let surface_terminal = completed_task
        + worker[completed_task..]
            .find("activity.publish_payload(SubagentActivityPayload::Completed")
            .expect("async worker must publish one normal surface terminal");
    assert!(
        schema < surface_terminal,
        "async output schema must be settled before surface terminal"
    );

    let continuation_terminal = worker
        .find("let projection = match commit_async_terminal(")
        .expect("async success path must commit continuation terminal");
    assert!(
        continuation_terminal < surface_terminal,
        "continuation terminal must be authoritative before surface terminal"
    );
}
