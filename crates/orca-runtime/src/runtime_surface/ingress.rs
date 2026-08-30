use std::io;
use std::sync::Arc;

use orca_core::plan_types::UpdatePlanArgs;
use orca_core::provider_types::ProviderStep;
use orca_core::thread_item_projection::ModelResponseIdentity;
use orca_core::tool_types::ToolResult;

use crate::child_agent_types::SubagentActivityEvent;
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

#[allow(private_interfaces)]
pub trait RuntimeWorkflowLifecycleIngress: Send + Sync + std::fmt::Debug {
    fn commit_started(
        &self,
        started: &RuntimeWorkflowStarted,
    ) -> io::Result<RuntimeWorkflowIngressReceipt>;
    fn commit_finished(&self, finished: &RuntimeWorkflowFinished) -> io::Result<()>;

    #[doc(hidden)]
    fn subagent_activity_ingress(&self) -> Option<Arc<dyn RuntimeSubagentActivityIngress>> {
        None
    }

    /// Replays detached child activity after an attachment or wake-up has
    /// established this operation fence. Implementations may reject when no
    /// actor-owned drain is available.
    #[doc(hidden)]
    fn drain_subagent_relay(&self, _task_id: &str, _attempt_id: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "detached subagent relay drain is unavailable",
        ))
    }
}

/// Actor-owned durable boundary for one ordered child-activity source event.
/// Implementations acknowledge only after the event's source commit id has
/// been committed or proven already committed by the runtime surface ledger.
pub trait RuntimeSubagentActivityIngress: Send + Sync + std::fmt::Debug {
    #[allow(private_interfaces)]
    fn owner(&self) -> crate::child_agent_types::SubagentActivityOwner;
    #[allow(private_interfaces)]
    fn commit_activity(&self, event: SubagentActivityEvent) -> io::Result<()>;
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
    fn commit_provider_steps(
        &self,
        identity: &ModelResponseIdentity,
        steps: &[ProviderStep],
    ) -> io::Result<()> {
        for step in steps {
            self.commit_provider_step(identity, step)?;
        }
        Ok(())
    }
    fn commit_tool_results(&self, results: &[ToolResult]) -> io::Result<()>;
    fn commit_plan_update(&self, _update: &UpdatePlanArgs) -> io::Result<()> {
        Ok(())
    }

    fn commit_tool_result(&self, result: &ToolResult) -> io::Result<()> {
        self.commit_tool_results(std::slice::from_ref(result))
    }
}
