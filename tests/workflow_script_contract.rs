use std::fs;

use orca_core::cost_types::UsageTotals;
use orca_core::workflow_types::{
    WorkflowAgentStatus, WorkflowEvidenceContract, WorkflowEvidenceIdentity,
    WorkflowEvidenceToolEvent, WorkflowInput, WorkflowMeta, WorkflowMutationPolicy,
    WorkflowPhaseRecord, WorkflowRunState, WorkflowRunStatus,
};
use orca_runtime::workflow::report::{render_evidence_markdown, render_report_for_run};
use orca_runtime::workflow::script::{
    parse_workflow_args_schema, resolve_workflow_script, resolve_workflow_script_to_path,
    resolve_workflow_script_with_user_dir_and_config_dir, validate_workflow_args,
};
use orca_runtime::workflow::state::{
    WorkflowAgentCacheRecord, WorkflowAgentRecord, WorkflowStateStore,
};
use orca_runtime::workflow::verifier::{WorkflowVerificationStatus, verify_workflow_run};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn inline_script_is_persisted_and_meta_is_extracted() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some("export const meta = { name: 'audit', description: 'Audit code', phases: ['scan', 'review'] };\nexport default await agent('inspect repo');".to_string()),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();

    assert_eq!(resolved.meta.name, "audit");
    assert_eq!(resolved.meta.description, "Audit code");
    assert_eq!(resolved.meta.phases, vec!["scan", "review"]);
    assert!(resolved.persisted_path.exists());
    assert!(
        fs::read_to_string(&resolved.persisted_path)
            .unwrap()
            .contains("export const meta")
    );
    assert_eq!(resolved.script_digest.len(), 64);
}

#[test]
fn inline_script_accepts_top_level_phase_objects() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'audit', description: 'Audit code' };\nexport const phases = [{ name: 'scan', tasks: [{ type: 'agent', prompt: 'inspect repo' }] }, { name: 'review', tasks: [] }];".to_string(),
        ),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();

    assert_eq!(resolved.meta.name, "audit");
    assert_eq!(resolved.meta.description, "Audit code");
    assert_eq!(resolved.meta.phases, vec!["scan", "review"]);
}

#[test]
fn workflow_meta_parses_optional_tags_and_version() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'audit', description: 'Audit code', phases: [], tags: ['security', 'audit'], version: '1' };\nexport default 'ok';"
                .to_string(),
        ),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();

    assert_eq!(resolved.meta.tags, vec!["security", "audit"]);
    assert_eq!(resolved.meta.version.as_deref(), Some("1"));
}

#[test]
fn workflow_scripts_can_be_persisted_per_run_with_same_meta_name() {
    let temp = tempdir().unwrap();
    let input = WorkflowInput {
        script: Some("export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default 'ok';".to_string()),
        ..Default::default()
    };
    let first_path = temp.path().join("session/workflow-runs/run-1/script.js");
    let second_path = temp.path().join("session/workflow-runs/run-2/script.js");

    let first = resolve_workflow_script_to_path(&input, temp.path(), &first_path).unwrap();
    let second = resolve_workflow_script_to_path(&input, temp.path(), &second_path).unwrap();

    assert_eq!(first.persisted_path, first_path);
    assert_eq!(second.persisted_path, second_path);
    assert_ne!(first.persisted_path, second.persisted_path);
    assert!(first.persisted_path.exists());
    assert!(second.persisted_path.exists());
}

#[test]
fn script_path_takes_precedence_over_inline_script() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let source = temp.path().join("chosen.js");
    fs::write(
        &source,
        "export const meta = { name: 'chosen', description: 'Chosen script', phases: [] };\nexport default 'ok';",
    )
    .unwrap();
    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'ignored', description: 'Ignored', phases: [] };"
                .to_string(),
        ),
        script_path: Some(source.display().to_string()),
        ..Default::default()
    };

    let resolved = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap();
    assert_eq!(resolved.meta.name, "chosen");
    assert_eq!(resolved.original_path.as_deref(), Some(source.as_path()));
}

#[test]
fn nearest_project_workflow_wins_over_user_workflow() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path().join("repo");
    let cwd = repo_root.join("packages/api");
    fs::create_dir_all(repo_root.join(".orca/workflows")).unwrap();
    fs::create_dir_all(temp.path().join("home/.orca/workflows")).unwrap();

    fs::write(
        repo_root.join(".orca/workflows/audit.js"),
        "export const meta = { name: 'audit', description: 'Project audit', phases: [] };\nexport default 'project';",
    )
    .unwrap();
    fs::write(
        temp.path().join("home/.orca/workflows/audit.js"),
        "export const meta = { name: 'audit', description: 'User audit', phases: [] };\nexport default 'user';",
    )
    .unwrap();
    orca_core::config::folder_trust::set_trust_with_config_dir(
        &repo_root,
        &temp.path().join("home/.orca"),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .unwrap();

    let input = WorkflowInput {
        name: Some("audit".to_string()),
        ..Default::default()
    };

    let resolved = resolve_workflow_script_with_user_dir_and_config_dir(
        &input,
        &cwd,
        &temp.path().join("session"),
        &temp.path().join("home/.orca/workflows"),
        &temp.path().join("home/.orca"),
    )
    .unwrap();

    assert_eq!(resolved.meta.description, "Project audit");
}

#[test]
fn workflow_args_schema_is_extracted_from_script() {
    let schema = parse_workflow_args_schema(
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\n\
         export const args = {\n\
           target: { type: 'string', required: true },\n\
           maxAgents: { type: 'number', required: false, default: 8 },\n\
           dryRun: { type: 'boolean', default: true }\n\
         };\n\
         export default args;",
    )
    .unwrap()
    .expect("args schema");

    assert_eq!(schema.len(), 3);
    assert!(schema["target"].required);
    assert_eq!(schema["target"].arg_type.as_str(), "string");
    assert_eq!(schema["maxAgents"].default, Some(json!(8)));
    assert_eq!(schema["dryRun"].arg_type.as_str(), "boolean");
}

#[test]
fn workflow_args_validation_applies_defaults_and_rejects_bad_inputs() {
    let schema = parse_workflow_args_schema(
        "export const args = {\n\
           target: { type: 'string', required: true },\n\
           maxAgents: { type: 'number', default: 8 },\n\
           dryRun: { type: 'boolean', default: false }\n\
         };",
    )
    .unwrap()
    .expect("args schema");

    let normalized = validate_workflow_args(Some(json!({ "target": "src" })), &schema).unwrap();
    assert_eq!(
        normalized,
        json!({ "target": "src", "maxAgents": 8, "dryRun": false })
    );

    let missing = validate_workflow_args(Some(json!({})), &schema).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        missing
            .to_string()
            .contains("missing required workflow arg `target`")
    );

    let wrong_type = validate_workflow_args(
        Some(json!({ "target": "src", "maxAgents": "many" })),
        &schema,
    )
    .unwrap_err();
    assert_eq!(wrong_type.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        wrong_type
            .to_string()
            .contains("workflow arg `maxAgents` must be number")
    );
}

#[test]
fn saved_workflow_args_schema_is_validated_before_launch_persistence() {
    let temp = tempdir().unwrap();
    let config_dir = temp.path().join("home");
    let workflow_dir = temp.path().join(".orca/workflows");
    fs::create_dir_all(&workflow_dir).unwrap();
    fs::write(
        workflow_dir.join("audit.js"),
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\n\
         export const args = { target: { type: 'string', required: true }, maxAgents: { type: 'number', default: 8 } };\n\
         export default args;",
    )
    .unwrap();
    orca_core::config::folder_trust::set_trust_with_config_dir(
        temp.path(),
        &config_dir,
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .unwrap();

    let input = WorkflowInput {
        name: Some("audit".to_string()),
        args: Some(json!({ "target": "src" })),
        ..Default::default()
    };

    let resolved = resolve_workflow_script_with_user_dir_and_config_dir(
        &input,
        temp.path(),
        &temp.path().join("session"),
        &config_dir.join("workflows"),
        &config_dir,
    )
    .expect("saved workflow resolves");
    let normalized = validate_workflow_args(input.args.clone(), &resolved.args_schema).unwrap();

    assert_eq!(resolved.args_schema.len(), 2);
    assert_eq!(normalized, json!({ "target": "src", "maxAgents": 8 }));
}

#[test]
fn missing_meta_fields_return_invalid_data_error() {
    let temp = tempdir().unwrap();
    let session_dir = temp.path().join("session");
    let input = WorkflowInput {
        script: Some(
            "export const meta = { name: 'audit', description: 'Audit code' };\nexport default 'x';"
                .to_string(),
        ),
        ..Default::default()
    };

    let error = resolve_workflow_script(&input, temp.path(), &session_dir).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn state_store_round_trips_run_state_and_agent_cache() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-1".to_string(),
        task_id: "task-1".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Queued,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: None,
        error: None,
    };

    store.create_run(&state).unwrap();
    assert!(store.transcript_dir(&state.run_id).exists());

    let loaded = store.load_run(&state.run_id).unwrap();
    assert_eq!(loaded.workflow_name, "audit");

    let updated_state = WorkflowRunState {
        status: WorkflowRunStatus::Completed,
        final_summary: Some("done".to_string()),
        ..loaded
    };
    store.write_state(&updated_state).unwrap();

    let record = WorkflowAgentCacheRecord {
        call_path: "phases.scan".to_string(),
        input_hash: "1234".to_string(),
        output: json!({
            "status": WorkflowAgentStatus::Completed,
            "summary": "cached"
        }),
    };
    store
        .record_agent_completed(&updated_state.run_id, record.clone())
        .unwrap();

    let cached = store
        .cached_agent_result(&updated_state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(cached, Some(record));
}

#[test]
fn workflow_evidence_bundle_round_trips_state_and_agent_rows() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-evidence".to_string(),
        task_id: "task-evidence".to_string(),
        session_id: "session-evidence".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string(), "review".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: vec![
            WorkflowPhaseRecord {
                name: "scan".to_string(),
                status: WorkflowRunStatus::Completed,
                started_at_ms: Some(100),
                completed_at_ms: Some(200),
                agent_count: 1,
                error: None,
                fallback: None,
            },
            WorkflowPhaseRecord {
                name: "review".to_string(),
                status: WorkflowRunStatus::Failed,
                started_at_ms: Some(210),
                completed_at_ms: Some(260),
                agent_count: 1,
                error: Some("review failed".to_string()),
                fallback: Some("continue".to_string()),
            },
        ],
        total_agent_count: 2,
        final_summary: Some("done with review fallback".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "call-scan".to_string(),
                call_path: "phases.scan:1".to_string(),
                prompt: "inspect repo".to_string(),
                opts: json!({ "team": "research" }),
                team: Some("research".to_string()),
                input_hash: "hash-scan".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 2,
                previous_errors: Vec::new(),
                output: Some(json!({ "summary": "ok" })),
                error: None,
                transcript_path: Some("/tmp/project/.orca/transcripts/call-scan.json".to_string()),
                started_at_ms: Some(110),
                completed_at_ms: Some(180),
                usage: Some(UsageTotals {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_tokens: 3,
                    estimated_cost_usd: 0.001,
                }),
                task: None,
                tool_events: Vec::new(),
                continuation: None,
            },
        )
        .unwrap();
    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "call-review".to_string(),
                call_path: "phases.review:1".to_string(),
                prompt: "review repo".to_string(),
                opts: json!({ "team": "review" }),
                team: Some("review".to_string()),
                input_hash: "hash-review".to_string(),
                status: WorkflowAgentStatus::Failed,
                attempt: 2,
                max_attempts: 2,
                previous_errors: vec!["transient timeout".to_string()],
                output: None,
                error: Some("review failed".to_string()),
                transcript_path: Some(
                    "/tmp/project/.orca/transcripts/call-review.json".to_string(),
                ),
                started_at_ms: Some(220),
                completed_at_ms: Some(255),
                usage: None,
                task: None,
                tool_events: Vec::new(),
                continuation: None,
            },
        )
        .unwrap();

    let bundle = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "0.0.0-test".to_string(),
                binary_path: Some("/tmp/orca".to_string()),
                generated_at_ms: 300,
            },
        )
        .unwrap();
    store.write_evidence_bundle(&bundle).unwrap();

    assert!(store.evidence_path(&state.run_id).exists());
    let loaded = store.load_evidence_bundle(&state.run_id).unwrap();
    assert_eq!(loaded.evidence_version, 1);
    assert_eq!(loaded.run_id, state.run_id);
    assert_eq!(loaded.task_id, state.task_id);
    assert_eq!(loaded.session_id, state.session_id);
    assert_eq!(loaded.cwd, state.cwd);
    assert_eq!(loaded.workflow_name, state.workflow_name);
    assert_eq!(loaded.script_digest, state.script_digest);
    assert_eq!(loaded.args_digest, state.args_digest);
    assert_eq!(loaded.status, WorkflowRunStatus::Completed);
    assert_eq!(loaded.total_agent_count, 2);
    assert_eq!(loaded.phases.len(), 2);
    assert_eq!(loaded.phases[1].error.as_deref(), Some("review failed"));
    assert_eq!(loaded.phases[1].fallback.as_deref(), Some("continue"));
    assert_eq!(loaded.agents.len(), 2);
    let scan = loaded
        .agents
        .iter()
        .find(|agent| agent.call_id == "call-scan")
        .expect("scan agent evidence");
    assert_eq!(scan.call_path, "phases.scan:1");
    assert_eq!(scan.team.as_deref(), Some("research"));
    assert_eq!(scan.status, WorkflowAgentStatus::Completed);
    assert_eq!(scan.attempt, 1);
    assert_eq!(scan.max_attempts, 2);
    assert_eq!(scan.input_hash, "hash-scan");
    assert_eq!(scan.usage.unwrap().total_tokens(), 30);
    let review = loaded
        .agents
        .iter()
        .find(|agent| agent.call_id == "call-review")
        .expect("review agent evidence");
    assert_eq!(review.previous_errors, vec!["transient timeout"]);
    assert_eq!(review.error.as_deref(), Some("review failed"));
}

#[test]
fn workflow_verifier_reports_proven_and_completed_with_failures_from_artifacts() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let mut state = WorkflowRunState {
        run_id: "workflow-run-verified".to_string(),
        task_id: "task-verified".to_string(),
        session_id: "session-verified".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: vec![WorkflowPhaseRecord {
            name: "scan".to_string(),
            status: WorkflowRunStatus::Completed,
            started_at_ms: Some(100),
            completed_at_ms: Some(200),
            agent_count: 1,
            error: None,
            fallback: None,
        }],
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();
    let transcript = store.transcript_dir(&state.run_id).join("agent-1.txt");
    fs::write(&transcript, "agent transcript").unwrap();
    fs::write(store.mailbox_path(&state.run_id), "{\"channels\":{}}").unwrap();
    fs::write(store.task_lists_path(&state.run_id), "{\"lists\":{}}").unwrap();
    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "agent-1".to_string(),
                call_path: "scan:1".to_string(),
                prompt: "inspect repo".to_string(),
                opts: json!({}),
                team: None,
                input_hash: "hash".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 1,
                previous_errors: Vec::new(),
                output: Some(json!("done")),
                error: None,
                transcript_path: Some(transcript.display().to_string()),
                started_at_ms: Some(100),
                completed_at_ms: Some(200),
                usage: None,
                task: None,
                tool_events: Vec::new(),
                continuation: None,
            },
        )
        .unwrap();
    let evidence = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "test".to_string(),
                binary_path: None,
                generated_at_ms: 300,
            },
        )
        .unwrap();
    store.write_evidence_bundle(&evidence).unwrap();

    let verified = verify_workflow_run(&store, &state.run_id).unwrap();
    assert_eq!(verified.status, WorkflowVerificationStatus::Proven);
    assert!(verified.mailbox_present);
    assert!(verified.task_lists_present);
    assert_eq!(verified.transcript_count, 1);

    state.run_id = "workflow-run-with-failures".to_string();
    state.task_id = "task-with-failures".to_string();
    state.phases[0].status = WorkflowRunStatus::Failed;
    state.phases[0].fallback = Some("continued".to_string());
    state.phases[0].error = Some("scan failed".to_string());
    store.create_run(&state).unwrap();
    let failed_evidence = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "test".to_string(),
                binary_path: None,
                generated_at_ms: 400,
            },
        )
        .unwrap();
    store.write_evidence_bundle(&failed_evidence).unwrap();

    let verified_with_failures = verify_workflow_run(&store, &state.run_id).unwrap();
    assert_eq!(
        verified_with_failures.status,
        WorkflowVerificationStatus::CompletedWithFailures
    );
    assert!(verified_with_failures.failure_count > 0);
}

#[test]
fn workflow_verifier_rejects_missing_declared_evidence_contract() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let mut state = WorkflowRunState {
        run_id: "workflow-run-contract".to_string(),
        task_id: "task-contract".to_string(),
        session_id: "session-contract".to_string(),
        cwd: temp.path().display().to_string(),
        workflow_name: "contract".to_string(),
        meta: WorkflowMeta {
            name: "contract".to_string(),
            description: "Contract test".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: vec![WorkflowPhaseRecord {
            name: "scan".to_string(),
            status: WorkflowRunStatus::Completed,
            started_at_ms: Some(100),
            completed_at_ms: Some(200),
            agent_count: 1,
            error: None,
            fallback: None,
        }],
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();
    let transcript = store.transcript_dir(&state.run_id).join("agent.txt");
    fs::write(&transcript, "agent transcript").unwrap();
    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "agent-1".to_string(),
                call_path: "scan:1".to_string(),
                prompt: "scan".to_string(),
                opts: json!({}),
                team: None,
                input_hash: "hash".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 1,
                previous_errors: Vec::new(),
                output: Some(json!("done")),
                error: None,
                transcript_path: Some(transcript.display().to_string()),
                started_at_ms: Some(100),
                completed_at_ms: Some(200),
                usage: None,
                task: None,
                tool_events: Vec::new(),
                continuation: None,
            },
        )
        .unwrap();
    let mut evidence = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "test".to_string(),
                binary_path: None,
                generated_at_ms: 300,
            },
        )
        .unwrap();
    evidence.contract = Some(WorkflowEvidenceContract {
        required_tool_calls: vec!["read_file".to_string()],
        expected_tool_failures: Vec::new(),
        expected_mcp_failures: Vec::new(),
        mutation_policy: Some(WorkflowMutationPolicy::ReadOnly),
        min_observed_concurrency: Some(2),
    });
    store.write_evidence_bundle(&evidence).unwrap();

    let verified = verify_workflow_run(&store, &state.run_id).unwrap();
    assert_eq!(verified.status, WorkflowVerificationStatus::NotProven);
    assert!(
        verified
            .contract_failures
            .iter()
            .any(|failure| failure.contains("required tool call `read_file` was not observed"))
    );
    assert!(
        verified
            .contract_failures
            .iter()
            .any(|failure| failure.contains("observed concurrency 0 is below required 2"))
    );

    state.run_id = "workflow-run-contract-proven".to_string();
    state.task_id = "task-contract-proven".to_string();
    store.create_run(&state).unwrap();
    let transcript = store.transcript_dir(&state.run_id).join("agent.txt");
    fs::write(&transcript, "agent transcript").unwrap();
    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "agent-1".to_string(),
                call_path: "scan:1".to_string(),
                prompt: "scan".to_string(),
                opts: json!({}),
                team: None,
                input_hash: "hash".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 1,
                previous_errors: Vec::new(),
                output: Some(json!("done")),
                error: None,
                transcript_path: Some(transcript.display().to_string()),
                started_at_ms: Some(100),
                completed_at_ms: Some(200),
                usage: None,
                task: None,
                tool_events: vec![WorkflowEvidenceToolEvent {
                    id: Some("tool-1".to_string()),
                    name: "read_file".to_string(),
                    status: Some("completed".to_string()),
                    target: Some("README.md".to_string()),
                    error: None,
                    is_mcp: false,
                }],
                continuation: None,
            },
        )
        .unwrap();
    let mut evidence = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "test".to_string(),
                binary_path: None,
                generated_at_ms: 400,
            },
        )
        .unwrap();
    evidence.contract = Some(WorkflowEvidenceContract {
        required_tool_calls: vec!["read_file".to_string()],
        expected_tool_failures: Vec::new(),
        expected_mcp_failures: Vec::new(),
        mutation_policy: Some(WorkflowMutationPolicy::ReadOnly),
        min_observed_concurrency: Some(1),
    });
    evidence.max_observed_concurrent_agents = 1;
    store.write_evidence_bundle(&evidence).unwrap();

    let verified = verify_workflow_run(&store, &state.run_id).unwrap();
    assert_eq!(verified.status, WorkflowVerificationStatus::Proven);
    assert!(verified.contract_failures.is_empty());
}

#[test]
fn workflow_verifier_rejects_read_only_contract_when_mutation_tool_completes() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-readonly-contract".to_string(),
        task_id: "task-readonly-contract".to_string(),
        session_id: "session-readonly-contract".to_string(),
        cwd: temp.path().display().to_string(),
        workflow_name: "readonly-contract".to_string(),
        meta: WorkflowMeta {
            name: "readonly-contract".to_string(),
            description: "Read-only contract test".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: vec![WorkflowPhaseRecord {
            name: "scan".to_string(),
            status: WorkflowRunStatus::Completed,
            started_at_ms: Some(100),
            completed_at_ms: Some(200),
            agent_count: 1,
            error: None,
            fallback: None,
        }],
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();
    let transcript = store.transcript_dir(&state.run_id).join("agent.txt");
    fs::write(&transcript, "agent transcript").unwrap();
    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "agent-1".to_string(),
                call_path: "scan:1".to_string(),
                prompt: "scan".to_string(),
                opts: json!({}),
                team: None,
                input_hash: "hash".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 1,
                previous_errors: Vec::new(),
                output: Some(json!("done")),
                error: None,
                transcript_path: Some(transcript.display().to_string()),
                started_at_ms: Some(100),
                completed_at_ms: Some(200),
                usage: None,
                task: None,
                tool_events: vec![WorkflowEvidenceToolEvent {
                    id: Some("tool-1".to_string()),
                    name: "edit".to_string(),
                    status: Some("completed".to_string()),
                    target: Some("src/lib.rs".to_string()),
                    error: None,
                    is_mcp: false,
                }],
                continuation: None,
            },
        )
        .unwrap();
    let mut evidence = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "test".to_string(),
                binary_path: None,
                generated_at_ms: 400,
            },
        )
        .unwrap();
    evidence.contract = Some(WorkflowEvidenceContract {
        required_tool_calls: Vec::new(),
        expected_tool_failures: Vec::new(),
        expected_mcp_failures: Vec::new(),
        mutation_policy: Some(WorkflowMutationPolicy::ReadOnly),
        min_observed_concurrency: None,
    });
    store.write_evidence_bundle(&evidence).unwrap();

    let verified = verify_workflow_run(&store, &state.run_id).unwrap();

    assert_eq!(verified.status, WorkflowVerificationStatus::NotProven);
    assert!(verified.contract_failures.iter().any(|failure| {
        failure.contains("read-only mutation policy") && failure.contains("edit")
    }));
}

#[test]
fn workflow_report_is_bound_to_evidence() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-report".to_string(),
        task_id: "task-report".to_string(),
        session_id: "session-report".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 3,
        final_summary: Some("evidence says three agents".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();
    for index in 0..3 {
        store
            .record_agent_completed(
                &state.run_id,
                WorkflowAgentRecord {
                    call_id: format!("call-{index}"),
                    call_path: format!("root:{index}"),
                    prompt: format!("agent {index}"),
                    opts: json!({}),
                    team: None,
                    input_hash: format!("hash-{index}"),
                    status: WorkflowAgentStatus::Completed,
                    attempt: 1,
                    max_attempts: 1,
                    previous_errors: Vec::new(),
                    output: Some(json!({ "index": index })),
                    error: None,
                    transcript_path: Some(format!("/tmp/transcript-{index}.json")),
                    started_at_ms: Some(100 + index),
                    completed_at_ms: Some(200 + index),
                    usage: None,
                    task: None,
                    tool_events: Vec::new(),
                    continuation: None,
                },
            )
            .unwrap();
    }
    let bundle = store
        .build_evidence_bundle(
            &state,
            WorkflowEvidenceIdentity {
                app_version: "0.0.0-test".to_string(),
                binary_path: None,
                generated_at_ms: 300,
            },
        )
        .unwrap();
    store.write_evidence_bundle(&bundle).unwrap();

    let markdown = render_evidence_markdown(&bundle);
    assert!(markdown.contains("workflow-run-report"));
    assert!(markdown.contains("| Total agents | 3 |"));
    assert!(!markdown.contains("| Total agents | 11 |"));

    let report = render_report_for_run(&store, &state.run_id).unwrap();
    assert!(report.markdown.contains("| Total agents | 3 |"));
    assert_eq!(report.json["totalAgentCount"], 3);
}

#[test]
fn workflow_report_blocks_without_verified_evidence() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-missing-evidence".to_string(),
        task_id: "task-missing-evidence".to_string(),
        session_id: "session-missing-evidence".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 11,
        final_summary: Some("should not be enough".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let error = render_report_for_run(&store, &state.run_id).unwrap_err();
    assert!(
        error.to_string().contains("no verified workflow evidence"),
        "unexpected error: {error}"
    );
}

#[test]
fn state_store_reads_legacy_agent_cache_shape() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy".to_string(),
        task_id: "task-legacy".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": {
                "status": "completed",
                "summary": "cached"
            }
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let cached = store
        .cached_agent_result(&state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(
        cached,
        Some(WorkflowAgentCacheRecord {
            call_path: "phases.scan".to_string(),
            input_hash: "1234".to_string(),
            output: json!({
                "status": "completed",
                "summary": "cached"
            }),
        })
    );

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(
        found,
        Some(json!({
            "status": "completed",
            "summary": "cached"
        }))
    );
}

#[test]
fn state_store_reads_legacy_string_agent_cache_without_json_quoting() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-string".to_string(),
        task_id: "task-legacy-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": "legacy result"
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(json!("legacy result")));
}

#[test]
fn state_store_preserves_legacy_json_looking_string_cache_values() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-json-string".to_string(),
        task_id: "task-legacy-json-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    for (input_hash, output) in [
        ("1234", "123"),
        ("false", "false"),
        ("null", "null"),
        ("array", "[1]"),
        ("object", "{\"k\":1}"),
    ] {
        let legacy_record = serde_json::json!({
            format!("phases.scan:{input_hash}"): {
                "call_path": "phases.scan",
                "input_hash": input_hash,
                "output": output
            }
        });
        fs::write(
            store.run_dir(&state.run_id).join("agent-cache.json"),
            serde_json::to_string_pretty(&legacy_record).unwrap(),
        )
        .unwrap();

        let found = store.find_cached_agent_value(&state.run_id, "phases.scan", input_hash);
        assert_eq!(found, Some(json!(output)), "case {output}");
    }
}

#[test]
fn state_store_reads_legacy_null_agent_cache_as_null_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-null".to_string(),
        task_id: "task-legacy-null".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(serde_json::Value::Null));
}

#[test]
fn state_store_returns_legacy_object_cache_as_object_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-legacy-object-value".to_string(),
        task_id: "task-legacy-object-value".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let legacy_record = serde_json::json!({
        "phases.scan:1234": {
            "call_path": "phases.scan",
            "input_hash": "1234",
            "output": {
                "kind": "legacy-object"
            }
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(json!({ "kind": "legacy-object" })));
}

#[test]
fn state_store_reads_current_null_output_as_null_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-null".to_string(),
        task_id: "task-current-null".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let current_record = serde_json::json!({
        "phases.scan:1234": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "1234",
            "status": "completed",
            "output": null,
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&current_record).unwrap(),
    )
    .unwrap();

    let found = store.find_cached_agent_value(&state.run_id, "phases.scan", "1234");
    assert_eq!(found, Some(serde_json::Value::Null));

    let cached = store
        .cached_agent_result(&state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(
        cached,
        Some(WorkflowAgentCacheRecord {
            call_path: "phases.scan".to_string(),
            input_hash: "1234".to_string(),
            output: serde_json::Value::Null,
        })
    );
}

#[test]
fn state_store_reads_current_string_agent_output_as_string_value() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-string".to_string(),
        task_id: "task-current-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let current_record = serde_json::json!({
        "phases.scan:1234": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "1234",
            "status": "completed",
            "output": "current result",
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&current_record).unwrap(),
    )
    .unwrap();

    let cached = store
        .cached_agent_result(&state.run_id, "phases.scan", "1234")
        .unwrap();
    assert_eq!(
        cached,
        Some(WorkflowAgentCacheRecord {
            call_path: "phases.scan".to_string(),
            input_hash: "1234".to_string(),
            output: json!("current result"),
        })
    );
}

#[test]
fn state_store_preserves_current_json_looking_string_outputs() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-json-string".to_string(),
        task_id: "task-current-json-string".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 1,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    for (index, input_hash, output) in [
        (1usize, "1234", "123"),
        (2, "false", "false"),
        (3, "null", "null"),
        (4, "array", "[1]"),
        (5, "object", "{\"k\":1}"),
    ] {
        store
            .record_agent_completed(
                &state.run_id,
                WorkflowAgentRecord {
                    call_id: format!("call-{index}"),
                    call_path: "phases.scan".to_string(),
                    prompt: "inspect repo".to_string(),
                    opts: json!(null),
                    team: None,
                    input_hash: input_hash.to_string(),
                    status: WorkflowAgentStatus::Completed,
                    attempt: 1,
                    max_attempts: 1,
                    previous_errors: Vec::new(),
                    output: Some(json!(output)),
                    error: None,
                    transcript_path: None,
                    started_at_ms: Some(1_000 + index as i64),
                    completed_at_ms: Some(2_000 + index as i64),
                    usage: None,
                    task: None,
                    tool_events: Vec::new(),
                    continuation: None,
                },
            )
            .unwrap();

        let found = store.find_cached_agent_value(&state.run_id, "phases.scan", input_hash);
        assert_eq!(found, Some(json!(output)), "find case {output}");

        let cached = store
            .cached_agent_result(&state.run_id, "phases.scan", input_hash)
            .unwrap();
        assert_eq!(
            cached,
            Some(WorkflowAgentCacheRecord {
                call_path: "phases.scan".to_string(),
                input_hash: input_hash.to_string(),
                output: json!(output),
            }),
            "cached case {output}"
        );
    }
}

#[test]
fn state_store_preserves_missing_output_field_when_appending_completed_record() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-missing-output-fidelity".to_string(),
        task_id: "task-missing-output-fidelity".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 2,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let cache_path = store.run_dir(&state.run_id).join("agent-cache.json");
    let current_records = serde_json::json!({
        "phases.scan:missing-output": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "missing-output",
            "status": "completed",
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&current_records).unwrap(),
    )
    .unwrap();

    store
        .record_agent_completed(
            &state.run_id,
            WorkflowAgentRecord {
                call_id: "call-2".to_string(),
                call_path: "phases.scan".to_string(),
                prompt: "inspect repo again".to_string(),
                opts: json!(null),
                team: None,
                input_hash: "with-output".to_string(),
                status: WorkflowAgentStatus::Completed,
                attempt: 1,
                max_attempts: 1,
                previous_errors: Vec::new(),
                output: Some(json!("cached result")),
                error: None,
                transcript_path: None,
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                usage: None,
                task: None,
                tool_events: Vec::new(),
                continuation: None,
            },
        )
        .unwrap();

    assert_eq!(
        store.find_cached_agent_value(&state.run_id, "phases.scan", "missing-output"),
        None
    );
    assert_eq!(
        store
            .cached_agent_result(&state.run_id, "phases.scan", "missing-output")
            .unwrap(),
        None
    );

    let rewritten_cache: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).unwrap()).unwrap();
    let original = &rewritten_cache["phases.scan:missing-output"];
    assert!(
        original.get("output").is_none(),
        "missing output field should stay omitted after cache rewrite: {original}"
    );
    assert_eq!(
        rewritten_cache["phases.scan:with-output"]["output"],
        json!("cached result")
    );
}

#[test]
fn state_store_cached_agent_result_ignores_incomplete_or_outputless_current_records() {
    let temp = tempdir().unwrap();
    let store = WorkflowStateStore::new(temp.path().join("runs"));
    let state = WorkflowRunState {
        run_id: "workflow-run-current-incomplete".to_string(),
        task_id: "task-current-incomplete".to_string(),
        session_id: "session-1".to_string(),
        cwd: "/tmp/project".to_string(),
        workflow_name: "audit".to_string(),
        meta: WorkflowMeta {
            name: "audit".to_string(),
            description: "Audit code".to_string(),
            phases: vec!["scan".to_string()],
            tags: Vec::new(),
            version: None,
        },
        script_digest: "abcd".repeat(16),
        args_digest: "ef01".repeat(16),
        status: WorkflowRunStatus::Completed,
        phases: Vec::new(),
        total_agent_count: 2,
        final_summary: Some("done".to_string()),
        error: None,
    };
    store.create_run(&state).unwrap();

    let current_records = serde_json::json!({
        "phases.scan:in-progress": {
            "callId": "call-1",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "in-progress",
            "status": "running",
            "output": "result",
            "error": null,
            "transcriptPath": null
        },
        "phases.scan:missing-output": {
            "callId": "call-2",
            "callPath": "phases.scan",
            "prompt": "inspect repo",
            "opts": null,
            "inputHash": "missing-output",
            "status": "completed",
            "error": null,
            "transcriptPath": null
        }
    });
    fs::write(
        store.run_dir(&state.run_id).join("agent-cache.json"),
        serde_json::to_string_pretty(&current_records).unwrap(),
    )
    .unwrap();

    assert_eq!(
        store
            .cached_agent_result(&state.run_id, "phases.scan", "in-progress")
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .cached_agent_result(&state.run_id, "phases.scan", "missing-output")
            .unwrap(),
        None
    );
    assert_eq!(
        store.find_cached_agent_value(&state.run_id, "phases.scan", "missing-output"),
        None
    );
}
