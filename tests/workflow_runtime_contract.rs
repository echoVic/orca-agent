use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
    WorkflowTeamConfig,
};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::model::ModelSelection;
use orca_core::task_types::TaskStatus;
use orca_core::workflow_types::{
    WorkflowAgentFailureKind, WorkflowAgentStatus, WorkflowEvidenceFailureKind, WorkflowRunStatus,
    WorkflowSourceMutationRisk,
};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::report::render_report_for_run;
use orca_runtime::workflow::state::{WorkflowStateStore, input_hash};
use orca_runtime::workflow::{WorkflowDraftStore, WorkflowLaunchRequest, WorkflowRunner};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn workflow_runner_executes_agent_and_writes_state() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo'));\nexport default result;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let record = tasks.get(&launched.task_id).unwrap();
    assert_eq!(record.status, orca_core::task_types::TaskStatus::Completed);
    let result = record.result.unwrap();
    assert!(result.contains("inspect repo"));
    assert!(
        !result.contains("Mock child agent completed prompt:"),
        "workflow runner should use the real child-agent executor path"
    );
    assert!(launched.output.script_path.unwrap().ends_with(".js"));
    assert!(
        launched
            .output
            .transcript_dir
            .unwrap()
            .contains("transcripts")
    );

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).unwrap();
    assert_eq!(evidence.run_id, run_id);
    assert_eq!(evidence.task_id, launched.task_id);
    assert_eq!(evidence.workflow_name, "audit");
    assert_eq!(evidence.status, WorkflowRunStatus::Completed);
    assert_eq!(evidence.total_agent_count, 1);
    assert_eq!(evidence.identity.app_version, "0.0.0-test");
    assert_eq!(evidence.agents.len(), 1);
    assert_eq!(evidence.agents[0].status, WorkflowAgentStatus::Completed);
    let task = evidence.agents[0]
        .task
        .as_ref()
        .expect("workflow child task lifecycle");
    assert_eq!(task.kind, "subagent");
    assert_eq!(task.status, "succeeded");
    assert_eq!(task.turn, 1);
    assert!(
        task.task_id.starts_with("workflow-child-"),
        "task id should use the workflow child run id: {}",
        task.task_id
    );
    assert!(
        evidence.agents[0]
            .transcript_path
            .as_deref()
            .unwrap_or_default()
            .contains("transcripts")
    );
}

#[test]
fn workflow_launch_result_includes_evidence_derived_status_line() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'status-line', description: 'Status line', phases: ['scan'] };\nexport default await phase('scan', async () => agent('inspect repo'));",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_concurrent_agents = 4;
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("inspect repo"));
    assert!(launched.status_line.contains("Workflow completed"));
    assert!(
        launched
            .status_line
            .contains("Agents: 1 completed, 0 failed, 0 running")
    );
    assert!(
        launched
            .status_line
            .contains("Phases: 1 completed, 0 failed")
    );
    assert!(
        launched
            .status_line
            .contains("Max observed concurrency: 1 / 4")
    );
    assert!(
        launched
            .status_line
            .contains("Inspect: /workflows -> workflow-run-")
    );
}

#[test]
fn workflow_draft_persists_preview_and_launches_from_draft() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let script = "export const meta = { name: 'draft-audit', description: 'Draft audit', phases: ['scan', 'report'] };\n\
const scan = await phase('scan', async () => parallel([\n\
  agent('inspect auth', { team: 'scanner' }),\n\
  agent('inspect permissions', { team: 'scanner' })\n\
]));\n\
export default await phase('report', async () => agent(`report ${JSON.stringify(scan)}`, { team: 'reporter' }));";

    let config = mock_run_config(temp.path());
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = draft_store
        .create_from_script(
            "session-1",
            temp.path(),
            script,
            config.workflows.max_concurrent_agents,
        )
        .expect("draft is created");

    assert!(draft.draft_id.starts_with("workflow-draft-"));
    assert_eq!(draft.session_id, "session-1");
    assert_eq!(draft.name, "draft-audit");
    assert_eq!(draft.description, "Draft audit");
    assert_eq!(draft.phases, vec!["scan", "report"]);
    assert_eq!(draft.estimated_agent_count, Some(3));
    assert_eq!(
        draft.max_configured_concurrent_agents,
        config.workflows.max_concurrent_agents as u32
    );
    assert_eq!(
        draft.source_mutation_risk,
        WorkflowSourceMutationRisk::ReadOnlyLikely
    );
    assert!(draft.script_path.ends_with("script.js"));

    let reloaded = draft_store
        .load(&draft.draft_id)
        .expect("draft reloads from disk");
    assert_eq!(reloaded, draft);
    assert_eq!(
        fs::read_to_string(draft_store.script_path(&draft.draft_id)).unwrap(),
        script
    );

    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_draft_id(draft.draft_id.clone()))
        .expect("draft launches");

    assert_eq!(
        launched.output.workflow_name.as_deref(),
        Some("draft-audit")
    );
    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let launch_input = store.load_launch_input(run_id).expect("launch input");
    assert_eq!(
        launch_input.draft_id.as_deref(),
        Some(draft.draft_id.as_str())
    );
    assert_eq!(launch_input.script_path, None);
    assert_eq!(launch_input.script, None);
    assert_eq!(
        store.load_run(run_id).unwrap().status,
        WorkflowRunStatus::Completed
    );
}

#[test]
fn workflow_draft_store_saves_reusable_workflow_and_cancels_draft() {
    let temp = tempdir().unwrap();
    let draft_store = WorkflowDraftStore::new(temp.path().join("session").join("workflow-drafts"));
    let script = "export const meta = { name: 'security-audit', description: 'Audit', phases: ['scan'] };\nexport default await phase('scan', async () => agent('scan repo'));";
    let draft = draft_store
        .create_from_script("session-1", temp.path(), script, 8)
        .expect("draft is created");

    let saved_path = draft_store
        .save_reusable(
            &draft.draft_id,
            &temp.path().join(".orca").join("workflows"),
            None,
        )
        .expect("workflow is saved");
    assert_eq!(saved_path.file_name().unwrap(), "security-audit.js");
    assert_eq!(fs::read_to_string(saved_path).unwrap(), script);

    draft_store
        .cancel(&draft.draft_id)
        .expect("draft is cancelled");
    assert!(!draft_store.draft_dir(&draft.draft_id).exists());
}

#[test]
fn workflow_draft_store_edits_script_and_reparses_preview_metadata() {
    let temp = tempdir().unwrap();
    let draft_store = WorkflowDraftStore::new(temp.path().join("session").join("workflow-drafts"));
    let draft = draft_store
        .create_from_script(
            "session-1",
            temp.path(),
            "export const meta = { name: 'audit', description: 'Audit', phases: ['scan'] };\nexport default 'old';",
            8,
        )
        .expect("draft is created");

    let edited = draft_store
        .edit_script(
            &draft.draft_id,
            "export const meta = { name: 'audit-v2', description: 'Edited audit', phases: ['scan', 'report'] };\nexport default 'new';",
            8,
        )
        .expect("draft is edited");

    assert_eq!(edited.draft_id, draft.draft_id);
    assert_eq!(edited.name, "audit-v2");
    assert_eq!(edited.description, "Edited audit");
    assert_eq!(edited.phases, vec!["scan", "report"]);
    assert_eq!(edited.estimated_agent_count, None);
    assert_eq!(
        fs::read_to_string(draft_store.script_path(&draft.draft_id)).unwrap(),
        edited.script
    );
}

#[test]
fn workflow_runner_applies_exported_args_schema_defaults_to_runtime_args() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'args-defaults', description: 'Args defaults', phases: [] };\n\
         export const args = { target: { type: 'string', required: true }, maxAgents: { type: 'number', default: 8 } };\n\
         export default args;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from(
            orca_core::workflow_types::WorkflowInput {
                script_path: Some(script.display().to_string()),
                args: Some(json!({ "target": "src" })),
                ..Default::default()
            },
        ))
        .expect("workflow launches with schema defaults");

    assert!(launched.summary.contains("\"target\":\"src\""));
    assert!(launched.summary.contains("\"maxAgents\":8"));
    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let launch_input = store.load_launch_input(run_id).expect("launch input");
    assert_eq!(
        launch_input.args,
        Some(json!({ "target": "src", "maxAgents": 8 }))
    );

    let summary = tasks
        .list()
        .into_iter()
        .find(|task| task.workflow_run_id.as_deref() == Some(run_id))
        .expect("workflow task summary");
    assert_eq!(
        summary.workflow_script_path.as_deref(),
        launched.output.script_path.as_deref()
    );
    assert_eq!(summary.workflow_launch_input, Some(launch_input));
    assert_eq!(summary.workflow_final_summary, launched.output.summary);
    assert_eq!(summary.workflow_failure_count, 0);
}

#[test]
fn workflow_resume_uses_completed_agent_cache() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'cache', description: 'Cache test', phases: [] };\nconst result = await agent('inspect repo');\nexport default result;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(first.output.run_id.clone().unwrap()),
        )
        .unwrap();

    assert!(second.summary.contains("cached 1 agent"));
}

#[test]
fn workflow_respects_configured_concurrency_in_evidence() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let repo = std::env::current_dir().expect("repo cwd");
    let script = repo.join("tests/fixtures/workflows/stress-evidence.js");
    assert!(script.exists(), "stress workflow fixture should exist");

    let temp = tempdir().unwrap();
    let mut config = mock_run_config(temp.path());
    config.workflows.max_concurrent_agents = 4;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("stress workflow runs");
    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");

    assert_eq!(evidence.status, WorkflowRunStatus::Completed);
    assert_eq!(evidence.total_agent_count, 16);
    assert_eq!(evidence.agents.len(), 16);
    assert_eq!(evidence.max_configured_concurrent_agents, 4);
    assert!(
        evidence.max_observed_concurrent_agents <= 4,
        "observed {} should stay within configured cap",
        evidence.max_observed_concurrent_agents
    );
    assert!(
        evidence
            .agents
            .iter()
            .all(|agent| agent.status == WorkflowAgentStatus::Completed)
    );
}

#[test]
fn workflow_barrier_min_hold_makes_max_concurrency_observable() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("barrier.js");
    fs::write(
        &script,
        "export const meta = { name: 'barrier', description: 'Barrier concurrency', phases: ['fanout'] };\n\
         export default await phase('fanout', async () => parallel([\n\
           agent('hold-1', { barrier: 'concurrency-4', minHoldMs: 150 }),\n\
           agent('hold-2', { barrier: 'concurrency-4', minHoldMs: 150 }),\n\
           agent('hold-3', { barrier: 'concurrency-4', minHoldMs: 150 }),\n\
           agent('hold-4', { barrier: 'concurrency-4', minHoldMs: 150 })\n\
         ]));",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_concurrent_agents = 4;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());

    let started = Instant::now();
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("barrier workflow runs");
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "minHoldMs should hold barrier agents long enough to make concurrency observable"
    );
    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");

    assert_eq!(evidence.status, WorkflowRunStatus::Completed);
    assert_eq!(evidence.max_configured_concurrent_agents, 4);
    assert!(
        evidence.max_observed_concurrent_agents > 1,
        "minHoldMs should make overlapping agents observable, saw {}",
        evidence.max_observed_concurrent_agents
    );
    assert!(
        evidence.max_observed_concurrent_agents <= 4,
        "observed {} should stay within configured cap",
        evidence.max_observed_concurrent_agents
    );
    assert!(
        evidence
            .agents
            .iter()
            .all(|agent| agent.barrier.as_deref() == Some("concurrency-4"))
    );
    assert!(
        evidence
            .agents
            .iter()
            .all(|agent| agent.min_hold_ms == Some(150))
    );
}

#[test]
fn workflow_resume_reuses_evidence_bound_cached_agents() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let repo = std::env::current_dir().expect("repo cwd");
    let script = repo.join("tests/fixtures/workflows/stress-evidence.js");
    assert!(script.exists(), "stress workflow fixture should exist");

    let temp = tempdir().unwrap();
    let mut config = mock_run_config(temp.path());
    config.workflows.max_concurrent_agents = 4;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("first stress workflow runs");
    let first_run_id = first.output.run_id.clone().expect("first run id");
    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(first_run_id),
        )
        .expect("resumed stress workflow runs from cache");

    assert!(second.summary.contains("cached 16 agents"));
    let run_id = second.output.run_id.as_deref().expect("second run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    assert_eq!(evidence.total_agent_count, 16);
    assert_eq!(evidence.agents.len(), 16);
    assert!(
        evidence
            .agents
            .iter()
            .all(|agent| agent.status == WorkflowAgentStatus::Cached)
    );
    assert_eq!(evidence.max_configured_concurrent_agents, 4);
    assert_eq!(evidence.max_observed_concurrent_agents, 0);
}

#[test]
fn workflow_resume_replays_legacy_object_cache_as_object_value() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'legacy-cache', description: 'Legacy cache test', phases: [] };\nconst result = await agent('inspect repo');\nexport default result.kind;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let resumed_run_id = first.output.run_id.clone().unwrap();
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let cache_path = store.run_dir(&resumed_run_id).join("agent-cache.json");
    let legacy_hash = input_hash("inspect repo", &json!({}));
    let legacy_record = json!({
        format!("root:1:{legacy_hash}"): {
            "call_path": "root:1",
            "input_hash": legacy_hash,
            "output": {
                "kind": "legacy-object"
            }
        }
    });
    fs::write(
        cache_path,
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(resumed_run_id),
        )
        .unwrap();

    let record = tasks.get(&second.task_id).unwrap();
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.result.as_deref(), Some("legacy-object"));
    assert!(second.summary.contains("cached 1 agent"));
}

#[test]
fn workflow_resume_reuses_complex_fallback_agents_as_cached_rows() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'resume-stress', description: 'Resume stress test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('mock_fail'), { fallback: async ({ error }) => agent(`recover ${error.includes('mock child failure requested')}`) });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.prompt}`));\n\
         export default { scan: scan.prompt, review: review.result };",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("first workflow completes with fallback");
    let first_run_id = first.output.run_id.clone().expect("first run id");
    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(first_run_id),
        )
        .expect("resumed workflow completes with cached fallback agents");
    let second_run_id = second.output.run_id.clone().expect("second run id");
    let third = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(second_run_id),
        )
        .expect("second-generation resume reuses cached rows");

    assert!(second.summary.contains("cached 2 agents"));
    assert!(second.summary.contains("review recovered=recover true"));
    assert!(third.summary.contains("cached 2 agents"));

    let record = tasks.get(&second.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.total_agents, 3);
    assert_eq!(progress.completed_agents, 2);
    assert_eq!(progress.failed_agents, 1);
    assert_eq!(progress.failed_phases, 1);
    assert_eq!(record.workflow_agents.len(), 3);

    let agent_statuses = record
        .workflow_agents
        .iter()
        .map(|agent| (agent.call_path.as_str(), agent.status))
        .collect::<Vec<_>>();
    assert!(agent_statuses.contains(&("scan:1", WorkflowAgentStatus::Failed)));
    assert!(agent_statuses.contains(&("scan:2", WorkflowAgentStatus::Cached)));
    assert!(agent_statuses.contains(&("review:3", WorkflowAgentStatus::Cached)));

    let run_id = second.output.run_id.as_deref().expect("resumed run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("resumed run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].fallback.as_deref(), Some("function"));
    assert_eq!(state.phases[0].agent_count, 2);
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
}

#[test]
fn workflow_child_agents_can_exchange_messages_with_mailbox_tools() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
         "export const meta = { name: 'child-mailbox', description: 'Child mailbox test', phases: ['scan', 'review'] };\n\
         await phase('scan', async () => agent('workflow_send_message findings scanner high'));\n\
         const review = await phase('review', async () => agent('workflow_read_messages findings'));\n\
         await phase('cleanup', async () => agent('workflow_clear_messages findings'));\n\
         const after = await phase('verify', async () => agent('workflow_read_messages findings'));\n\
         export default { before: review.result, after: after.result };",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.approval_mode = ApprovalMode::AutoEdit;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir);

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with child mailbox tools");

    assert!(launched.summary.contains("scanner"));
    assert!(launched.summary.contains("high"));
    assert!(launched.summary.contains("[]"));
    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.workflow_agents.len(), 4);
}

#[test]
fn workflow_child_agents_can_claim_and_complete_shared_tasks() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'child-tasks', description: 'Child task list test', phases: ['setup', 'work', 'verify'] };\n\
         await phase('setup', async () => agent('workflow_create_task_list audit api docs'));\n\
         await phase('work', async () => agent('workflow_claim_task audit worker-a'));\n\
         await phase('done', async () => agent('workflow_complete_task audit workflow-task-1 worker-a ok'));\n\
         const tasks = await phase('verify', async () => agent('workflow_list_tasks audit'));\n\
         export default tasks.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.approval_mode = ApprovalMode::AutoEdit;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir);

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with child task-list tools");

    assert!(launched.summary.contains("workflow-task-1"));
    assert!(launched.summary.contains("completed"));
    assert!(launched.summary.contains("worker-a"));
    assert!(launched.summary.contains("api"));
    assert!(launched.summary.contains("ok"));
    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.workflow_agents.len(), 4);
}

#[test]
fn workflow_runner_marks_task_and_run_failed_on_host_error() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'host-failure', description: 'Host failure test', phases: [] };\nthrow new Error('boom from workflow');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error.to_string().contains("boom from workflow"),
        "unexpected host error: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert!(record.error.as_deref().is_some());

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert!(state.error.as_deref().is_some());
}

#[test]
fn workflow_runner_marks_task_and_run_failed_on_child_agent_error() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'child-failure', description: 'Child failure test', phases: [] };\nawait agent('mock_fail');\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(error.to_string().contains("mock child failure requested"));

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert!(
        record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("mock child failure requested")
    );

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("mock child failure requested")
    );
}

#[test]
fn workflow_runner_retries_transient_child_agent_failure() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let flaky_prompt = format!("mock_flaky_once {}", uuid::Uuid::new_v4());
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        format!(
            "export const meta = {{ name: 'child-retry', description: 'Child retry test', phases: [] }};\nconst result = await agent({});\nexport default result.result;",
            serde_json::to_string(&flaky_prompt).unwrap()
        ),
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("Mock runtime completed"));

    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.total_agents, 2);
    assert_eq!(progress.running_agents, 0);
    assert_eq!(progress.completed_agents, 1);
    assert_eq!(progress.failed_agents, 1);
    assert_eq!(record.workflow_agents.len(), 1);
    assert_eq!(record.workflow_agents[0].call_path, "root:1");
    assert_eq!(
        record.workflow_agents[0].status,
        orca_core::workflow_types::WorkflowAgentStatus::Completed
    );
    assert_eq!(record.workflow_agents[0].attempt, 2);
    assert_eq!(record.workflow_agents[0].max_attempts, 2);
    assert_eq!(record.workflow_agents[0].previous_errors.len(), 1);

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.total_agent_count, 2);
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    assert_eq!(evidence.total_agent_count, 2);
    assert_eq!(evidence.agents.len(), 1);
    assert_eq!(evidence.agents[0].attempt, 2);
    assert_eq!(evidence.agents[0].max_attempts, 2);
    assert_eq!(evidence.agents[0].previous_errors.len(), 1);
    assert_eq!(evidence.agents[0].retryable, Some(true));
    assert!(evidence.agents[0].retry_attempted);
    assert_eq!(
        evidence.agents[0].failure_kind,
        Some(WorkflowAgentFailureKind::AgentFailed)
    );
    assert!(
        store
            .cached_agent_result(run_id, "root:1", &input_hash(&flaky_prompt, &json!({})))
            .unwrap()
            .is_some()
    );
    let cache_path = store.run_dir(run_id).join("agent-cache.json");
    let cache_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cache_path).unwrap()).unwrap();
    let cache_key = format!("root:1:{}", input_hash(&flaky_prompt, &json!({})));
    let record = &cache_json[cache_key];
    assert_eq!(record["attempt"], 2);
    assert_eq!(record["maxAttempts"], 2);
    assert_eq!(record["previousErrors"].as_array().unwrap().len(), 1);
}

#[test]
fn workflow_agent_summary_surfaces_token_usage() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'usage', description: 'Usage test', phases: [] };\n\
         export default await agent('mock_usage');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("usage workflow runs");

    let record = tasks.get(&launched.task_id).expect("task record");
    let usage = record.workflow_agents[0]
        .usage
        .expect("agent usage should be surfaced");
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.cache_tokens, 10);
}

#[test]
fn workflow_agent_summary_surfaces_team_from_agent_options() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'teams', description: 'Team test', phases: [] };\n\
         await Promise.all([\n\
           agent('inspect api', { team: 'backend' }),\n\
           agent('inspect ui', { team: 'frontend' })\n\
         ]);\n\
         export default 'done';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("team workflow runs");

    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.workflow_agents.len(), 2);
    let teams = record
        .workflow_agents
        .iter()
        .map(|agent| (agent.call_path.as_str(), agent.team.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(
        teams,
        vec![("root:1", Some("backend")), ("root:2", Some("frontend")),]
    );

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let summaries = store.agent_summaries(run_id).expect("agent summaries");
    assert_eq!(summaries[0].team.as_deref(), Some("backend"));
    assert_eq!(summaries[1].team.as_deref(), Some("frontend"));
}

#[test]
fn workflow_team_policy_enforces_team_token_budget() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'team-policy', description: 'Team policy test', phases: [] };\n\
         export default await agent('mock_usage', { team: 'backend' });",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.teams.insert(
        "backend".to_string(),
        WorkflowTeamConfig {
            max_agent_retries: Some(0),
            max_agent_tokens: Some(100),
            allowed_tools: None,
        },
    );
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("150 tokens exceeded per-agent token budget 100"),
        "team token budget should fail backend agent: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(agent.team.as_deref(), Some("backend"));
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
    assert_eq!(agent.attempt, 1);
    assert_eq!(agent.max_attempts, 1);
    assert_eq!(
        agent
            .usage
            .expect("usage should be preserved")
            .total_tokens(),
        150
    );
}

#[test]
fn workflow_team_policy_blocks_disallowed_tools() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'team-tools', description: 'Team tool policy test', phases: [] };\n\
         export default await agent('bash printf hi', { team: 'backend' });",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.teams.insert(
        "backend".to_string(),
        WorkflowTeamConfig {
            max_agent_retries: Some(0),
            max_agent_tokens: None,
            allowed_tools: Some(vec!["read_file".to_string()]),
        },
    );
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("workflow team 'backend' disallows tool 'bash'"),
        "team tool policy should fail backend agent: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(agent.team.as_deref(), Some("backend"));
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    assert_eq!(evidence.agents.len(), 1);
    assert_eq!(
        evidence.agents[0].failure_kind,
        Some(WorkflowAgentFailureKind::ToolFailure)
    );
    assert_eq!(evidence.agents[0].retryable, Some(false));
    assert!(!evidence.agents[0].retry_attempted);
    assert!(evidence.failures.iter().any(|failure| failure.kind
        == WorkflowEvidenceFailureKind::AgentFailed
        && failure.call_id.as_deref() == Some(evidence.agents[0].call_id.as_str())));
}

#[test]
fn workflow_evidence_classifies_mcp_tool_failures_without_stopping_independent_branches() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'mcp-diagnostics', description: 'MCP diagnostics test', phases: ['fanout'] };\n\
         const results = await phase('fanout', async () => parallel([\n\
           agent('mcp__broken__tool', { team: 'mcp' }),\n\
           agent('inspect still runs', { team: 'control' })\n\
         ]), { fallback: 'continue' });\n\
         export default 'continued after mcp failure';",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("fallback keeps workflow running after mcp branch fails");

    assert!(launched.summary.contains("continued after mcp failure"));
    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    assert_eq!(evidence.status, WorkflowRunStatus::Completed);
    assert_eq!(evidence.agents.len(), 2);
    let mcp_agent = evidence
        .agents
        .iter()
        .find(|agent| agent.team.as_deref() == Some("mcp"))
        .expect("mcp agent evidence");
    assert_eq!(mcp_agent.status, WorkflowAgentStatus::Failed);
    assert_eq!(
        mcp_agent.failure_kind,
        Some(WorkflowAgentFailureKind::McpFailure)
    );
    assert_eq!(mcp_agent.retryable, Some(false));
    assert!(!mcp_agent.retry_attempted);
    let control_agent = evidence
        .agents
        .iter()
        .find(|agent| agent.team.as_deref() == Some("control"))
        .expect("control agent evidence");
    assert_eq!(control_agent.status, WorkflowAgentStatus::Completed);
    assert!(evidence.failures.iter().any(|failure| failure.kind
        == WorkflowEvidenceFailureKind::PhaseFailedContinue
        && failure.phase_name.as_deref() == Some("fanout")));
}

#[test]
fn workflow_evidence_classifies_tool_policy_failures_without_stopping_independent_branches() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'tool-diagnostics', description: 'Tool diagnostics test', phases: ['fanout'] };\n\
         await phase('fanout', async () => parallel([\n\
           agent('bash printf hi', { team: 'blocked' }),\n\
           agent('inspect still runs', { team: 'control' })\n\
         ]), { fallback: 'continue' });\n\
         export default 'continued after tool failure';",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    config.workflows.teams.insert(
        "blocked".to_string(),
        WorkflowTeamConfig {
            max_agent_retries: Some(0),
            max_agent_tokens: None,
            allowed_tools: Some(vec!["read_file".to_string()]),
        },
    );
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("fallback keeps workflow running after tool branch fails");

    assert!(launched.summary.contains("continued after tool failure"));
    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    assert_eq!(evidence.status, WorkflowRunStatus::Completed);
    assert_eq!(evidence.agents.len(), 2);
    let blocked_agent = evidence
        .agents
        .iter()
        .find(|agent| agent.team.as_deref() == Some("blocked"))
        .expect("blocked agent evidence");
    assert_eq!(blocked_agent.status, WorkflowAgentStatus::Failed);
    assert_eq!(
        blocked_agent.failure_kind,
        Some(WorkflowAgentFailureKind::ToolFailure)
    );
    assert_eq!(blocked_agent.retryable, Some(false));
    assert!(!blocked_agent.retry_attempted);
    let control_agent = evidence
        .agents
        .iter()
        .find(|agent| agent.team.as_deref() == Some("control"))
        .expect("control agent evidence");
    assert_eq!(control_agent.status, WorkflowAgentStatus::Completed);
    assert!(evidence.failures.iter().any(|failure| failure.kind
        == WorkflowEvidenceFailureKind::PhaseFailedContinue
        && failure.phase_name.as_deref() == Some("fanout")));
}

#[test]
fn workflow_agent_token_budget_fails_agent_after_usage_exceeds_limit() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'usage-budget', description: 'Usage budget test', phases: [] };\n\
         export default await agent('mock_usage');",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_tokens = Some(100);
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("exceeded per-agent token budget"),
        "error should explain the budget failure: {error}"
    );
    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
    assert_eq!(agent.attempt, 1);
    assert_eq!(agent.max_attempts, 2);
    assert!(agent.previous_errors.is_empty());
    assert_eq!(
        agent
            .usage
            .expect("usage should be preserved")
            .total_tokens(),
        150
    );
    assert!(
        agent
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("150 tokens exceeded per-agent token budget 100")
    );

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("exceeded per-agent token budget")
    );
}

#[test]
fn workflow_agent_schema_failure_fails_agent_and_run() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'schema-failure', description: 'Schema failure test', phases: [] };\n\
         export default await agent('mock_usage', {\n\
           schema: {\n\
             type: 'object',\n\
             required: ['result'],\n\
             properties: { result: { type: 'object' } }\n\
           }\n\
         });",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("workflow agent output schema validation failed"),
        "error should explain schema validation failure: {error}"
    );
    assert!(
        error.to_string().contains("result"),
        "schema error should identify the failing property: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
    assert!(
        agent
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("workflow agent output schema validation failed")
    );

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
}

#[test]
fn workflow_agent_schema_accepts_matching_output() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'schema-success', description: 'Schema success test', phases: [] };\n\
         const result = await agent('mock_usage', {\n\
           schema: {\n\
             type: 'object',\n\
             required: ['prompt', 'result'],\n\
             properties: {\n\
               prompt: { type: 'string' },\n\
               result: { type: 'string' }\n\
             }\n\
           }\n\
         });\n\
         export default `${result.prompt}:${typeof result.result}`;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("schema-matching workflow runs");

    assert_eq!(launched.summary, "mock_usage:string");
    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.workflow_agents.len(), 1);
    assert_eq!(
        record.workflow_agents[0].status,
        orca_core::workflow_types::WorkflowAgentStatus::Completed
    );
}

#[test]
fn workflow_agent_worktree_isolation_preserves_parent_checkout() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let repo = tempdir().unwrap();
    run_git(repo.path(), &["init"]);
    run_git(repo.path(), &["config", "user.email", "orca@example.test"]);
    run_git(repo.path(), &["config", "user.name", "Orca Test"]);
    fs::write(repo.path().join("file.txt"), "placeholder").unwrap();
    run_git(repo.path(), &["add", "file.txt"]);
    run_git(repo.path(), &["commit", "-m", "seed"]);

    let script = repo.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'workflow-worktree', description: 'Worktree test', phases: [] };\n\
         export default await agent('edit file.txt :: placeholder => child', { isolation: 'worktree' });",
    )
    .unwrap();

    let config = mock_run_config(repo.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), repo.path().join("session"));
    runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow runs");

    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "placeholder"
    );
    let changed_worktree = fs::read_dir(repo.path().join(".orca/worktrees"))
        .expect("worktree directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("file.txt").exists())
        .expect("dirty workflow worktree was preserved");
    assert_eq!(
        fs::read_to_string(changed_worktree.join("file.txt")).unwrap(),
        "child"
    );
}

#[test]
fn parallel_preserves_order_and_records_phase() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'parallel', description: 'Parallel test', phases: ['fanout'] };\nconst result = await phase('fanout', async () => parallel([agent('first'), agent('second')]));\nexport default result.map(item => item.prompt).join(',');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("first,second"));

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.total_agent_count, 2);
    assert_eq!(state.phases.len(), 1);
    let phase = &state.phases[0];
    assert_eq!(phase.name, "fanout");
    assert_eq!(phase.status, WorkflowRunStatus::Completed);
    assert_eq!(phase.agent_count, 2);
    assert!(phase.started_at_ms.is_some());
    assert!(phase.completed_at_ms.is_some());

    assert!(
        store
            .cached_agent_result(run_id, "fanout:1", &input_hash("first", &json!({})))
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .cached_agent_result(run_id, "fanout:2", &input_hash("second", &json!({})))
            .unwrap()
            .is_some()
    );
}

#[test]
fn workflow_runner_persists_marker_phase_for_following_agents() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'marker-runtime', description: 'Marker runtime test', phases: ['scan'] };\nphase('scan');\nawait agent('inspect repo');\nexport default 'done';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.phases.len(), 1);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].agent_count, 1);
}

#[test]
fn failing_phase_is_persisted_as_failed_and_completed() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'failing-phase', description: 'Failing phase test', phases: ['scan'] };\nawait phase('scan', async () => { await agent('inspect repo'); throw new Error('boom in phase'); });\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let err = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(err.to_string().contains("boom in phase"));

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases.len(), 1);
    let phase = &state.phases[0];
    assert_eq!(phase.name, "scan");
    assert_eq!(phase.status, WorkflowRunStatus::Failed);
    assert_eq!(phase.agent_count, 1);
    assert!(phase.started_at_ms.is_some());
    assert!(phase.completed_at_ms.is_some());
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    let failure_kinds = evidence
        .failures
        .iter()
        .map(|failure| failure.kind)
        .collect::<Vec<_>>();
    assert!(failure_kinds.contains(&WorkflowEvidenceFailureKind::PhaseFailedBlocked));
    assert!(failure_kinds.contains(&WorkflowEvidenceFailureKind::WorkflowFailed));
}

#[test]
fn phase_fallback_continue_records_failed_phase_and_runs_next_phase() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'phase-fallback', description: 'Phase fallback test', phases: ['scan', 'review'] };\n\
         await phase('scan', async () => agent('mock_fail'), { fallback: 'continue' });\n\
         const review = await phase('review', async () => agent('review anyway'));\n\
         export default review.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with fallback");

    assert!(launched.summary.contains("review anyway"));
    let record = tasks.get(&launched.task_id).expect("task record");
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.completed_phases, 1);
    assert_eq!(progress.failed_phases, 1);
    assert_eq!(progress.failed_agents, 1);

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases.len(), 2);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].agent_count, 1);
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    assert_eq!(evidence.status, WorkflowRunStatus::Completed);
    assert!(evidence.failures.iter().any(|failure| failure.kind
        == WorkflowEvidenceFailureKind::PhaseFailedContinue
        && failure.phase_name.as_deref() == Some("scan")));
    assert!(
        evidence
            .failures
            .iter()
            .any(|failure| failure.kind == WorkflowEvidenceFailureKind::AgentFailed)
    );
    let report = render_report_for_run(&store, run_id).expect("evidence report");
    assert!(report.markdown.contains("| Status | completed |"));
    assert!(report.markdown.contains("phase_failed_continue"));
    assert!(report.markdown.contains("agent_failed"));
}

#[test]
fn phase_fallback_value_is_returned_and_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'phase-fallback-value', description: 'Phase fallback value test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('mock_fail'), { fallback: { value: { recovered: true, label: 'fallback-value' } } });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.recovered}`));\n\
         export default review.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with fallback value");

    assert!(launched.summary.contains("review recovered=true"));
    let record = tasks.get(&launched.task_id).expect("task record");
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.completed_phases, 1);
    assert_eq!(progress.failed_phases, 1);

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].fallback.as_deref(), Some("value"));
    assert!(
        state.phases[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("mock child failure requested")
    );
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
}

#[test]
fn phase_fallback_function_runs_recovery_agent_and_is_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'phase-fallback-function', description: 'Phase fallback function test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('mock_fail'), { fallback: async ({ error }) => agent(`recover ${error.includes('mock child failure requested')}`) });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.prompt}`));\n\
         export default review.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with fallback function");

    assert!(launched.summary.contains("review recovered=recover true"));
    let record = tasks.get(&launched.task_id).expect("task record");
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.completed_phases, 1);
    assert_eq!(progress.failed_phases, 1);
    assert_eq!(progress.failed_agents, 1);

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].fallback.as_deref(), Some("function"));
    assert_eq!(state.phases[0].agent_count, 2);
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
}

#[test]
fn workflow_summary_prefers_child_result_over_agent_prompt() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'summary', description: 'Summary test', phases: [] };\nexport default await agent('review this');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("review this"));
    assert_ne!(launched.summary, "review this");
}

#[test]
fn workflow_summary_uses_last_dsl_phase_result() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dsl-summary', description: 'DSL summary', phases: [{ name: 'roles', tasks: [{ prompt: 'role draft' }] }, { name: 'synthesis', tasks: [{ prompt: 'final plan' }] }] };",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("final plan"));

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(temp.path().join("session").join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.total_agent_count, 2);
    assert_eq!(state.phases.len(), 2);
    assert_eq!(state.phases[0].name, "roles");
    assert_eq!(state.phases[0].agent_count, 1);
    assert_eq!(state.phases[1].name, "synthesis");
    assert_eq!(state.phases[1].agent_count, 1);
    assert!(
        state
            .final_summary
            .as_deref()
            .unwrap_or_default()
            .contains("final plan")
    );
}

#[test]
fn workflow_runner_streams_running_phase_progress_to_state() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'live-state', description: 'Live state test', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo'));\nexport default result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.hooks = vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "sleep 1.0".to_string(),
        tool: None,
    }];
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let task_id = launch.task_id.clone();

    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let run_id = launch.output.run_id.as_deref().expect("run id");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut live_state = None;
    let mut live_task = None;
    while std::time::Instant::now() < deadline {
        let state = store.load_run(run_id).expect("run state");
        if state.status == WorkflowRunStatus::Running && state.total_agent_count == 1 {
            if let Some(task) = tasks.get(&task_id)
                && task.workflow_agents.iter().any(|agent| {
                    agent.call_path == "scan:1"
                        && agent.status == WorkflowAgentStatus::Running
                        && agent.started_at_ms.is_some()
                        && agent.completed_at_ms.is_none()
                })
            {
                live_state = Some(state);
                live_task = Some(task);
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }

    let live_state = live_state.expect("running state should include live phase progress");
    assert_eq!(live_state.phases.len(), 1);
    assert_eq!(live_state.phases[0].name, "scan");
    assert_eq!(live_state.phases[0].status, WorkflowRunStatus::Running);
    assert_eq!(live_state.phases[0].agent_count, 1);
    let live_task = live_task.expect("task summary should include running agent row");
    let live_agent = live_task
        .workflow_agents
        .iter()
        .find(|agent| agent.call_path == "scan:1")
        .expect("running agent summary");
    assert_eq!(live_agent.status, WorkflowAgentStatus::Running);
    assert!(live_agent.started_at_ms.is_some());
    assert_eq!(live_agent.completed_at_ms, None);

    launch.join().unwrap().unwrap();

    let completed_task = tasks.get(&task_id).expect("completed task");
    let completed_agent = completed_task
        .workflow_agents
        .iter()
        .find(|agent| agent.call_path == "scan:1")
        .expect("completed agent summary");
    assert_eq!(completed_agent.status, WorkflowAgentStatus::Completed);
    assert!(completed_agent.started_at_ms.is_some());
    assert!(completed_agent.completed_at_ms.is_some());
}

#[test]
fn workflow_runner_stops_when_control_file_is_requested() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'stop-control', description: 'Stop control test', phases: [] };\nawait agent('first');\nexport default await agent('second');",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.hooks = vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "sleep 1.0".to_string(),
        tool: None,
    }];
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    thread::sleep(Duration::from_millis(250));
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    store
        .request_stop(launch.output.run_id.as_deref().unwrap())
        .expect("request stop");

    let result = launch.join().unwrap().unwrap();

    assert_eq!(result.output.status, "stopped");
    let record = tasks.get(&result.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Stopped);
    let state = store
        .load_run(result.output.run_id.as_deref().unwrap())
        .expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Stopped);
}

#[test]
fn workflow_runner_stops_silent_host_when_control_file_is_requested() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'silent-stop', description: 'Silent stop test', phases: [] };\nwhile (true) {}\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-silent-stop".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let run_id = launch.output.run_id.as_deref().unwrap().to_string();
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));

    thread::sleep(Duration::from_millis(150));
    let started = Instant::now();
    store.request_stop(&run_id).expect("request silent stop");
    let result = launch.join().unwrap().unwrap();

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(result.output.status, "stopped");
    assert_eq!(
        tasks.get(&result.task_id).expect("task record").status,
        TaskStatus::Stopped
    );
    assert_eq!(
        store.load_run(&run_id).expect("run state").status,
        WorkflowRunStatus::Stopped
    );
}

#[test]
fn workflow_runner_stops_during_agent_min_hold() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'held-stop', description: 'Held stop test', phases: [] };\nexport default await agent('held', { minHoldMs: 30000 });",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-held-stop".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let run_id = launch.output.run_id.as_deref().unwrap().to_string();
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if tasks.get(&launch.task_id).is_some_and(|task| {
            task.workflow_agents
                .iter()
                .any(|agent| agent.status == WorkflowAgentStatus::Running)
        }) {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let started = Instant::now();
    store.request_stop(&run_id).expect("request held stop");
    let result = launch.join().unwrap().unwrap();

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(result.output.status, "stopped");
}

#[test]
fn workflow_runner_task_stop_interrupts_paused_agent() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'paused-stop', description: 'Paused stop test', phases: [] };\nexport default await agent('paused');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-paused-stop".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let task_id = launch.task_id.clone();
    let run_id = launch.output.run_id.as_deref().unwrap().to_string();
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    store.request_pause(&run_id).expect("request pause");

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if store
            .load_run(&run_id)
            .is_ok_and(|state| state.status == WorkflowRunStatus::Paused)
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        store.load_run(&run_id).expect("paused state").status,
        WorkflowRunStatus::Paused
    );

    let started = Instant::now();
    tasks.request_stop(&task_id).expect("request task stop");
    let result = launch.join().unwrap().unwrap();

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(result.output.status, "stopped");
}

#[test]
fn workflow_runner_cancels_unawaited_agent_after_terminal_event() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'terminal-agent', description: 'Terminal agent test', phases: [] };\nagent('unawaited', { minHoldMs: 30000 });\nexport default 'done';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-terminal-agent".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let started = Instant::now();
    let result = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("terminal event should cancel unawaited work");

    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(result.output.status, "completed");
    assert_eq!(result.summary, "done");
    assert_eq!(
        tasks
            .get(&result.task_id)
            .expect("terminal workflow task")
            .workflow_agents[0]
            .status,
        WorkflowAgentStatus::Cancelled
    );
}

#[test]
fn workflow_runner_pauses_and_resumes_from_control_file() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'pause-control', description: 'Pause control test', phases: [] };\nawait agent('first');\nexport default await agent('second');",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.hooks = vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "sleep 0.7".to_string(),
        tool: None,
    }];
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let task_id = launch.task_id.clone();
    let run_id = launch.output.run_id.as_deref().unwrap().to_string();
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));

    thread::sleep(Duration::from_millis(150));
    store.request_pause(&run_id).expect("request pause");

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let state = store.load_run(&run_id).expect("run state");
        let task = tasks.get(&task_id).expect("task record");
        if state.status == WorkflowRunStatus::Paused && task.status == TaskStatus::Paused {
            store.request_resume(&run_id).expect("request resume");
            let result = launch.join().unwrap().unwrap();
            assert_eq!(result.output.status, "completed");
            let completed_state = store.load_run(&run_id).expect("completed state");
            assert_eq!(completed_state.status, WorkflowRunStatus::Completed);
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }

    panic!("workflow did not enter paused state");
}

#[test]
fn workflow_draft_store_clones_run_script_and_args_as_draft() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'clone-me', description: 'Clone run', phases: [] };\nexport const args = { target: { type: 'string', required: true } };\nexport default args.target;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from(
            orca_core::workflow_types::WorkflowInput {
                script_path: Some(script.display().to_string()),
                args: Some(json!({ "target": "crates" })),
                ..Default::default()
            },
        ))
        .unwrap();

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let state_store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = draft_store
        .clone_from_run(&state_store, run_id, 8)
        .expect("clone run as draft");

    assert_eq!(draft.name, "clone-me");
    assert_eq!(draft.args, Some(json!({ "target": "crates" })));
    assert!(
        fs::read_to_string(draft_store.script_path(&draft.draft_id))
            .unwrap()
            .contains("export const args")
    );
}

#[test]
fn workflow_evidence_records_child_tool_events_from_jsonl() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello workflow").unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'tool-events', description: 'Tool event evidence', phases: [] };\nexport default await agent('read README.md');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let evidence = store.load_evidence_bundle(run_id).expect("evidence");
    let agent = evidence.agents.first().expect("agent evidence");
    assert!(
        agent
            .tool_events
            .iter()
            .any(|event| event.name == "read_file" && event.status.as_deref() == Some("completed")),
        "expected child read_file tool event in evidence: {:?}",
        agent.tool_events
    );
}

#[test]
fn agent_cap_failure_is_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'cap', description: 'Cap test', phases: [] };\nfor (let i = 0; i < 4; i++) await agent(`agent ${i}`);\nexport default 'unreachable';",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agents_per_run = 3;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let err = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("maximum workflow agent count 3 exceeded")
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert_eq!(state.total_agent_count, 3);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("maximum workflow agent count 3 exceeded")
    );
}

fn mock_run_config(cwd: &std::path::Path) -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        model_runtime: Default::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: Default::default(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: Default::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: Default::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
