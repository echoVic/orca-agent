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
const WORKFLOW_RUNNER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/workflow/runner.rs"
));
const HOSTED_CONTROLLER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orca-tui/src/hosted_controller.rs"
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
    // Keeping a TaskRegistry-only fallback creates two observability models:
    // one path reaches the parent surface and one path is merely a mirror.
    // Production child execution must either receive the surface ingress or
    // fail before starting, so a missing parent cannot silently hide events.
    let sink_selection = balanced_block(
        SYNC_SUBAGENT,
        "let activity_sink: Arc<dyn ChildAgentActivitySink>",
    );
    assert!(
        !sink_selection.contains("TaskRegistryActivitySink"),
        "sync child activity must not fall back to a registry-only mirror"
    );
}

#[test]
fn surface_task_projection_preserves_the_registry_parent_identity() {
    let function = balanced_block(
        GENERATION_ACTOR,
        "pub(super) fn commit_surface_subagent_activity",
    );
    assert!(
        function.contains("parent_task_id: active.task_registry")
            && function.contains("parent_task_id"),
        "the first child task projection must derive parent_task_id from the authoritative task registry"
    );
}

#[test]
fn durable_commit_retry_checks_the_event_digest_before_acknowledging_id() {
    let function = balanced_block(
        GENERATION_ACTOR,
        "pub(super) fn commit_surface_subagent_activity",
    );
    let lookup_start = function
        .find("lookup_commit(&event.surface_commit_id)")
        .expect("subagent activity commit should consult the durable retry index");
    let lookup_block = balanced_block(&function[lookup_start..], "if self");
    assert!(
        lookup_block.contains("batch_digest") && lookup_block.contains("event.digest"),
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
