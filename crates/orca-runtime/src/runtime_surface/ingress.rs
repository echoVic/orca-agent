use std::io;

use orca_core::plan_types::UpdatePlanArgs;
use orca_core::provider_types::ProviderStep;
use orca_core::thread_item_projection::ModelResponseIdentity;
use orca_core::tool_types::ToolResult;

use crate::model_response::RuntimeModelResponse;

use super::{
    DisplayText, NonEmptyText, SurfaceTaskFence, SurfaceTaskId, SurfaceToolCallId,
    SurfaceWorkflowFence, SurfaceWorkflowRunId, UnixMillis,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeWorkflowStarted {
    pub task_id: SurfaceTaskId,
    pub workflow_run_id: SurfaceWorkflowRunId,
    pub tool_call_id: SurfaceToolCallId,
    pub name: NonEmptyText,
    pub phases: Vec<NonEmptyText>,
    pub created_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeWorkflowIngressReceipt {
    pub workflow: SurfaceWorkflowFence,
    pub task: SurfaceTaskFence,
    pub tool_call_id: SurfaceToolCallId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeWorkflowOutcome {
    Completed { status_line: DisplayText },
    Failed { error: DisplayText },
    Cancelled { reason: DisplayText },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeWorkflowFinished {
    pub receipt: RuntimeWorkflowIngressReceipt,
    pub outcome: RuntimeWorkflowOutcome,
    pub completed_at: UnixMillis,
}

pub trait RuntimeWorkflowLifecycleIngress: Send + Sync + std::fmt::Debug {
    fn commit_started(
        &self,
        started: &RuntimeWorkflowStarted,
    ) -> io::Result<RuntimeWorkflowIngressReceipt>;
    fn commit_finished(&self, finished: &RuntimeWorkflowFinished) -> io::Result<()>;
}

pub trait RuntimeProviderResponseIngress: Send + Sync + std::fmt::Debug {
    fn commit_response(&self, response: &RuntimeModelResponse) -> io::Result<()>;
    fn commit_provider_attempt_failure(
        &self,
        _identity: &ModelResponseIdentity,
        _message: &str,
    ) -> io::Result<()> {
        Ok(())
    }
    fn commit_provider_failure(
        &self,
        _identity: &ModelResponseIdentity,
        _message: &str,
    ) -> io::Result<()> {
        Ok(())
    }
    fn commit_provider_step(
        &self,
        identity: &ModelResponseIdentity,
        step: &ProviderStep,
    ) -> io::Result<()>;
    fn commit_tool_results(&self, results: &[ToolResult]) -> io::Result<()>;
    fn commit_plan_update(&self, _update: &UpdatePlanArgs) -> io::Result<()> {
        Ok(())
    }

    fn commit_tool_result(&self, result: &ToolResult) -> io::Result<()> {
        self.commit_tool_results(std::slice::from_ref(result))
    }
}
