use std::io;

use orca_core::workflow_types::{
    WorkflowEvidenceBundle, WorkflowMutationPolicy, WorkflowRunStatus,
};
use serde::{Deserialize, Serialize};

use super::state::WorkflowStateStore;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowVerificationStatus {
    Proven,
    NotProven,
    Failed,
    CompletedWithFailures,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowVerificationReport {
    pub status: WorkflowVerificationStatus,
    pub run_id: String,
    pub evidence_status: WorkflowRunStatus,
    pub failure_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_failures: Vec<String>,
    pub mailbox_present: bool,
    pub task_lists_present: bool,
    pub transcript_count: u32,
    pub missing_transcript_count: u32,
}

pub fn verify_workflow_run(
    store: &WorkflowStateStore,
    run_id: &str,
) -> io::Result<WorkflowVerificationReport> {
    let evidence = store.load_evidence_bundle(run_id)?;
    let mailbox_present = artifact_is_readable(&store.mailbox_path(run_id));
    let task_lists_present = artifact_is_readable(&store.task_lists_path(run_id));
    let (transcript_count, missing_transcript_count) = transcript_counts(&evidence);
    let failure_count = evidence.failures.len() as u32;
    let contract_failures = contract_failures(&evidence);
    let status = verification_status(
        &evidence,
        failure_count,
        missing_transcript_count,
        contract_failures.len() as u32,
    );

    Ok(WorkflowVerificationReport {
        status,
        run_id: run_id.to_string(),
        evidence_status: evidence.status,
        failure_count,
        contract_failures,
        mailbox_present,
        task_lists_present,
        transcript_count,
        missing_transcript_count,
    })
}

fn artifact_is_readable(path: &std::path::Path) -> bool {
    path.exists() && std::fs::read_to_string(path).is_ok()
}

fn transcript_counts(evidence: &WorkflowEvidenceBundle) -> (u32, u32) {
    let mut present = 0u32;
    let mut missing = 0u32;
    for agent in &evidence.agents {
        let Some(path) = agent.transcript_path.as_deref() else {
            missing += 1;
            continue;
        };
        if artifact_is_readable(std::path::Path::new(path)) {
            present += 1;
        } else {
            missing += 1;
        }
    }
    (present, missing)
}

fn verification_status(
    evidence: &WorkflowEvidenceBundle,
    failure_count: u32,
    missing_transcript_count: u32,
    contract_failure_count: u32,
) -> WorkflowVerificationStatus {
    match evidence.status {
        WorkflowRunStatus::Failed | WorkflowRunStatus::Cancelled | WorkflowRunStatus::Stopped => {
            WorkflowVerificationStatus::Failed
        }
        WorkflowRunStatus::Completed if failure_count > 0 => {
            WorkflowVerificationStatus::CompletedWithFailures
        }
        WorkflowRunStatus::Completed if missing_transcript_count > 0 => {
            WorkflowVerificationStatus::NotProven
        }
        WorkflowRunStatus::Completed if contract_failure_count > 0 => {
            WorkflowVerificationStatus::NotProven
        }
        WorkflowRunStatus::Completed => WorkflowVerificationStatus::Proven,
        WorkflowRunStatus::Queued
        | WorkflowRunStatus::Running
        | WorkflowRunStatus::Paused
        | WorkflowRunStatus::Stopping
        | WorkflowRunStatus::AsyncLaunched => WorkflowVerificationStatus::NotProven,
    }
}

fn contract_failures(evidence: &WorkflowEvidenceBundle) -> Vec<String> {
    let Some(contract) = &evidence.contract else {
        return Vec::new();
    };
    let mut failures = Vec::new();
    let tool_events = evidence
        .agents
        .iter()
        .flat_map(|agent| agent.tool_events.iter())
        .collect::<Vec<_>>();

    for required in &contract.required_tool_calls {
        if !tool_events
            .iter()
            .any(|event| &event.name == required && event.status.as_deref() == Some("completed"))
        {
            failures.push(format!("required tool call `{required}` was not observed"));
        }
    }

    for expected in &contract.expected_tool_failures {
        if !tool_events.iter().any(|event| {
            &event.name == expected
                && !event.is_mcp
                && matches!(event.status.as_deref(), Some("failed") | Some("denied"))
        }) {
            failures.push(format!(
                "expected tool failure `{expected}` was not observed"
            ));
        }
    }

    for expected in &contract.expected_mcp_failures {
        if !tool_events.iter().any(|event| {
            &event.name == expected
                && event.is_mcp
                && matches!(event.status.as_deref(), Some("failed") | Some("denied"))
        }) {
            failures.push(format!(
                "expected MCP failure `{expected}` was not observed"
            ));
        }
    }

    if let Some(min) = contract.min_observed_concurrency
        && evidence.max_observed_concurrent_agents < min
    {
        failures.push(format!(
            "observed concurrency {} is below required {min}",
            evidence.max_observed_concurrent_agents
        ));
    }

    if contract.mutation_policy == Some(WorkflowMutationPolicy::ReadOnly) {
        for event in tool_events.iter().filter(|event| {
            event.file_effect_count > 0
                || (event.status.as_deref() == Some("completed")
                    && is_source_mutation_tool(&event.name))
        }) {
            failures.push(format!(
                "read-only mutation policy was violated by `{}` tool call{}",
                event.name,
                event
                    .target
                    .as_ref()
                    .map(|target| format!(" targeting `{target}`"))
                    .unwrap_or_default()
            ));
        }
    }

    failures
}

fn is_source_mutation_tool(name: &str) -> bool {
    matches!(name, "edit" | "write_file" | "apply_patch")
}

#[cfg(test)]
mod tests {
    #[test]
    fn completed_patches_count_as_workflow_mutations() {
        assert!(super::is_source_mutation_tool("apply_patch"));
    }
}
