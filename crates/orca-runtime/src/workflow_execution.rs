use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::tool_types;
use orca_core::workflow_types::{WorkflowDraftActionOutput, WorkflowInput};

use crate::agent_child::ChildAgentExecutor;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::runtime_surface::{
    DisplayText, NonEmptyText, RuntimeWorkflowFinished, RuntimeWorkflowIngressReceipt,
    RuntimeWorkflowLifecycleIngress, RuntimeWorkflowOutcome, RuntimeWorkflowStarted, SurfaceTaskId,
    SurfaceToolCallId, SurfaceWorkflowRunId, UnixMillis,
};
use crate::tasks::TaskRegistry;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow::{
    WorkflowBackgroundLaunch, WorkflowDraftStore, WorkflowLaunchRequest, WorkflowLaunchResult,
    WorkflowRunner,
};

const WORKFLOW_STARTUP_HEALTH_CHECK_POLLS: usize = 2;
const WORKFLOW_STARTUP_HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(300);
const WORKFLOW_COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub(crate) struct BackgroundWorkflowRun {
    pub(crate) task_id: String,
    pub(crate) run_id: String,
    pub(crate) workflow_name: String,
    pub(crate) task: crate::lifecycle::RuntimeTaskLifecycle,
    pub(crate) handle: WorkflowBackgroundLaunch,
    pub(crate) tool_use_id: Option<String>,
    pub(crate) ingress_receipt: Option<RuntimeWorkflowIngressReceipt>,
}

impl BackgroundWorkflowRun {
    pub(crate) fn new(launch: WorkflowBackgroundLaunch, tool_use_id: Option<String>) -> Self {
        let mut lifecycle = RuntimeSessionLifecycle::new(launch.run_id.clone());
        let task = lifecycle.start_task(RuntimeTaskKind::Workflow).clone();
        Self {
            task_id: launch.task_id.clone(),
            run_id: launch.run_id.clone(),
            workflow_name: launch.workflow_name.clone(),
            task,
            handle: launch,
            tool_use_id,
            ingress_receipt: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_ingress(
        launch: WorkflowBackgroundLaunch,
        tool_use_id: Option<String>,
        ingress_receipt: Option<RuntimeWorkflowIngressReceipt>,
    ) -> Self {
        let mut workflow = Self::new(launch, tool_use_id);
        workflow.ingress_receipt = ingress_receipt;
        workflow
    }

    pub(crate) fn join_silently(self) {
        let _ = self.handle.join();
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn invalid_workflow_identity(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn commit_workflow_started(
    ingress: Option<&dyn RuntimeWorkflowLifecycleIngress>,
    launch: &WorkflowBackgroundLaunch,
    tool_call_id: &str,
    task_registry: &TaskRegistry,
) -> io::Result<Option<RuntimeWorkflowIngressReceipt>> {
    let Some(ingress) = ingress else {
        return Ok(None);
    };
    let task = task_registry.get(&launch.task_id).ok_or_else(|| {
        invalid_workflow_identity("workflow task disappeared before typed start commit")
    })?;
    let started = RuntimeWorkflowStarted {
        task_id: SurfaceTaskId::try_new(launch.task_id.clone())
            .map_err(|_| invalid_workflow_identity("workflow task id is empty"))?,
        workflow_run_id: SurfaceWorkflowRunId::try_new(launch.run_id.clone())
            .map_err(|_| invalid_workflow_identity("workflow run id is empty"))?,
        tool_call_id: SurfaceToolCallId::try_new(tool_call_id.to_string())
            .map_err(|_| invalid_workflow_identity("workflow tool call id is empty"))?,
        name: NonEmptyText::try_new(launch.workflow_name.clone())
            .map_err(|_| invalid_workflow_identity("workflow name is empty"))?,
        phases: launch
            .phases
            .iter()
            .cloned()
            .map(|phase| {
                NonEmptyText::try_new(phase)
                    .map_err(|_| invalid_workflow_identity("workflow phase name is empty"))
            })
            .collect::<io::Result<Vec<_>>>()?,
        created_at: UnixMillis::new(task.created_at_ms),
    };
    ingress.commit_started(&started).map(Some)
}

fn commit_workflow_finished(
    ingress: Option<&dyn RuntimeWorkflowLifecycleIngress>,
    receipt: Option<RuntimeWorkflowIngressReceipt>,
    outcome: RuntimeWorkflowOutcome,
) -> io::Result<()> {
    match (ingress, receipt) {
        (Some(ingress), Some(receipt)) => ingress.commit_finished(&RuntimeWorkflowFinished {
            receipt,
            outcome,
            completed_at: UnixMillis::new(now_ms()),
        }),
        (None, None) => Ok(()),
        _ => Err(io::Error::other(
            "typed workflow lifecycle ingress lost its start receipt",
        )),
    }
}

fn reject_unwaited_typed_workflow(
    tool_request: &tool_types::ToolRequest,
    wait_for_background_workflows: bool,
    ingress: Option<&dyn RuntimeWorkflowLifecycleIngress>,
) -> Option<tool_types::ToolResult> {
    (ingress.is_some() && !wait_for_background_workflows).then(|| {
        tool_types::ToolResult::failed_before_start(
            tool_request,
            "typed workflow background transfer is unavailable; wait for workflow completion",
            None,
        )
    })
}

enum WorkflowStartupStatus {
    StillRunning(WorkflowBackgroundLaunch),
    Completed(WorkflowLaunchResult),
    Failed { error: String },
}

fn wait_for_workflow_startup(launch: WorkflowBackgroundLaunch) -> WorkflowStartupStatus {
    let mut launch = Some(launch);
    for _ in 0..WORKFLOW_STARTUP_HEALTH_CHECK_POLLS {
        if launch
            .as_ref()
            .is_some_and(WorkflowBackgroundLaunch::is_finished)
        {
            break;
        }
        thread::sleep(WORKFLOW_STARTUP_HEALTH_CHECK_INTERVAL);
    }

    let launch = launch.take().expect("launch present");
    if !launch.is_finished() {
        return WorkflowStartupStatus::StillRunning(launch);
    }

    match launch.join() {
        Ok(Ok(result)) => WorkflowStartupStatus::Completed(result),
        Ok(Err(error)) => WorkflowStartupStatus::Failed {
            error: error.to_string(),
        },
        Err(_) => WorkflowStartupStatus::Failed {
            error: "workflow thread panicked".to_string(),
        },
    }
}

fn emit_workflow_completed<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    task: &crate::lifecycle::RuntimeTaskLifecycle,
    task_id: &str,
    run_id: &str,
    workflow_name: &str,
    status_line: &str,
) -> io::Result<()> {
    let completed_task = task.with_status(RuntimeTaskStatus::Succeeded);
    let completed_event =
        completed_task.attach_to_event(events.workflow_completed(task_id, run_id, workflow_name));
    sink.emit(completed_event)?;
    let result_event = completed_task.attach_to_event(events.workflow_result_available(
        task_id,
        run_id,
        workflow_name,
        None,
        "completed",
        status_line,
    ));
    sink.emit(result_event)
}

fn emit_workflow_failed<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    task: &crate::lifecycle::RuntimeTaskLifecycle,
    task_id: &str,
    run_id: &str,
    workflow_name: &str,
    error: &str,
) -> io::Result<()> {
    let failed_task = task.with_status(RuntimeTaskStatus::Failed);
    let event = failed_task.attach_to_event(events.workflow_failed(
        task_id,
        run_id,
        workflow_name,
        None,
        error,
    ));
    sink.emit(event)
}

fn completed_workflow_result(
    tool_request: &tool_types::ToolRequest,
    result: WorkflowLaunchResult,
) -> io::Result<tool_types::ToolResult> {
    let output = serde_json::to_string(&result.output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(tool_types::ToolResult::completed(
        tool_request,
        output,
        false,
    ))
}

pub(crate) fn execute_workflow_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    child_executor: ChildAgentExecutor<SharedEventBuffer>,
    wait_for_background_workflows: bool,
    workflow_ingress: Option<&dyn RuntimeWorkflowLifecycleIngress>,
) -> io::Result<tool_types::ToolResult> {
    if !config.workflows.enabled {
        return Ok(tool_types::ToolResult::failed(
            tool_request,
            "workflows are disabled",
            None,
        ));
    }
    if let Some(result) = reject_unwaited_typed_workflow(
        tool_request,
        wait_for_background_workflows,
        workflow_ingress,
    ) {
        return Ok(result);
    }

    let input = parse_workflow_input(tool_request)?;
    let session_dir = task_registry.workflow_session_dir(cwd)?;
    let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir)
        .with_child_executor(child_executor);
    let launch = runner.launch_background(WorkflowLaunchRequest::from(input))?;
    let task_id = launch.task_id.clone();
    let run_id = launch.run_id.clone();
    let workflow_name = launch.workflow_name.clone();
    let mut workflow_lifecycle = RuntimeSessionLifecycle::new(launch.run_id.clone());
    let workflow_task = workflow_lifecycle
        .start_task(RuntimeTaskKind::Workflow)
        .clone();
    let ingress_receipt =
        match commit_workflow_started(workflow_ingress, &launch, &tool_request.id, task_registry) {
            Ok(receipt) => receipt,
            Err(error) => {
                let _ = task_registry.request_stop(&launch.task_id);
                let _ = launch.join();
                return Err(error);
            }
        };
    if emit_deltas {
        let event = workflow_task.attach_to_event(events.workflow_started(
            &launch.task_id,
            &launch.run_id,
            &launch.workflow_name,
            &launch.phases,
        ));
        sink.emit(event)?;
    }

    match wait_for_workflow_startup(launch) {
        WorkflowStartupStatus::StillRunning(launch) => {
            let output = serde_json::to_string(&launch.output)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            background_workflows.push(BackgroundWorkflowRun {
                task_id: launch.task_id.clone(),
                run_id: launch.run_id.clone(),
                workflow_name: launch.workflow_name.clone(),
                task: workflow_task,
                handle: launch,
                tool_use_id: Some(tool_request.id.clone()),
                ingress_receipt,
            });
            Ok(tool_types::ToolResult::completed(
                tool_request,
                output,
                false,
            ))
        }
        WorkflowStartupStatus::Completed(result) => {
            commit_workflow_finished(
                workflow_ingress,
                ingress_receipt,
                RuntimeWorkflowOutcome::Completed {
                    status_line: DisplayText::new(result.status_line.clone()),
                },
            )?;
            if emit_deltas {
                emit_workflow_completed(
                    events,
                    sink,
                    &workflow_task,
                    &task_id,
                    &run_id,
                    &workflow_name,
                    &result.status_line,
                )?;
            }
            completed_workflow_result(tool_request, result)
        }
        WorkflowStartupStatus::Failed { error } => {
            commit_workflow_finished(
                workflow_ingress,
                ingress_receipt,
                RuntimeWorkflowOutcome::Failed {
                    error: DisplayText::new(error.clone()),
                },
            )?;
            if emit_deltas {
                emit_workflow_failed(
                    events,
                    sink,
                    &workflow_task,
                    &task_id,
                    &run_id,
                    &workflow_name,
                    &error,
                )?;
            }
            Ok(tool_types::ToolResult::failed(tool_request, error, None))
        }
    }
}

pub(crate) fn execute_workflow_draft_action_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    child_executor: ChildAgentExecutor<SharedEventBuffer>,
    wait_for_background_workflows: bool,
    workflow_ingress: Option<&dyn RuntimeWorkflowLifecycleIngress>,
) -> io::Result<tool_types::ToolResult> {
    if !config.workflows.enabled {
        return Ok(tool_types::ToolResult::failed(
            tool_request,
            "workflows are disabled",
            None,
        ));
    }

    let input = parse_workflow_draft_action_input(tool_request)?;
    if input.action == "run"
        && let Some(result) = reject_unwaited_typed_workflow(
            tool_request,
            wait_for_background_workflows,
            workflow_ingress,
        )
    {
        return Ok(result);
    }
    let session_dir = task_registry.workflow_session_dir(cwd)?;
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = draft_store.load(&input.draft_id)?;

    let output = match input.action.as_str() {
        "run" => {
            let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir)
                .with_child_executor(child_executor);
            let launch = runner.launch_background(WorkflowLaunchRequest::from(WorkflowInput {
                draft_id: Some(input.draft_id.clone()),
                args: input.args.clone(),
                token_budget: input.token_budget,
                ..Default::default()
            }))?;
            let task_id = launch.task_id.clone();
            let run_id = launch.run_id.clone();
            let workflow_name = launch.workflow_name.clone();
            let mut workflow_lifecycle = RuntimeSessionLifecycle::new(launch.run_id.clone());
            let workflow_task = workflow_lifecycle
                .start_task(RuntimeTaskKind::Workflow)
                .clone();
            let ingress_receipt = match commit_workflow_started(
                workflow_ingress,
                &launch,
                &tool_request.id,
                task_registry,
            ) {
                Ok(receipt) => receipt,
                Err(error) => {
                    let _ = task_registry.request_stop(&launch.task_id);
                    let _ = launch.join();
                    return Err(error);
                }
            };
            if emit_deltas {
                let event = workflow_task.attach_to_event(events.workflow_started(
                    &launch.task_id,
                    &launch.run_id,
                    &launch.workflow_name,
                    &launch.phases,
                ));
                sink.emit(event)?;
            }
            match wait_for_workflow_startup(launch) {
                WorkflowStartupStatus::StillRunning(launch) => {
                    let action_output = WorkflowDraftActionOutput {
                        status: "async_launched".to_string(),
                        action: "run".to_string(),
                        draft_id: input.draft_id.clone(),
                        workflow_name: launch.workflow_name.clone(),
                        saved_path: None,
                        task_id: Some(launch.task_id.clone()),
                        run_id: Some(launch.run_id.clone()),
                        script_path: launch.output.script_path.clone(),
                    };
                    background_workflows.push(BackgroundWorkflowRun {
                        task_id: launch.task_id.clone(),
                        run_id: launch.run_id.clone(),
                        workflow_name: launch.workflow_name.clone(),
                        task: workflow_task,
                        handle: launch,
                        tool_use_id: Some(tool_request.id.clone()),
                        ingress_receipt,
                    });
                    action_output
                }
                WorkflowStartupStatus::Completed(result) => {
                    commit_workflow_finished(
                        workflow_ingress,
                        ingress_receipt,
                        RuntimeWorkflowOutcome::Completed {
                            status_line: DisplayText::new(result.status_line.clone()),
                        },
                    )?;
                    if emit_deltas {
                        emit_workflow_completed(
                            events,
                            sink,
                            &workflow_task,
                            &task_id,
                            &run_id,
                            &workflow_name,
                            &result.status_line,
                        )?;
                    }
                    WorkflowDraftActionOutput {
                        status: "completed".to_string(),
                        action: "run".to_string(),
                        draft_id: input.draft_id.clone(),
                        workflow_name: result
                            .output
                            .workflow_name
                            .clone()
                            .unwrap_or_else(|| workflow_name.clone()),
                        saved_path: None,
                        task_id: Some(result.task_id),
                        run_id: result.output.run_id,
                        script_path: result
                            .output
                            .script_path
                            .or_else(|| Some(draft.script_path.clone())),
                    }
                }
                WorkflowStartupStatus::Failed { error } => {
                    commit_workflow_finished(
                        workflow_ingress,
                        ingress_receipt,
                        RuntimeWorkflowOutcome::Failed {
                            error: DisplayText::new(error.clone()),
                        },
                    )?;
                    if emit_deltas {
                        emit_workflow_failed(
                            events,
                            sink,
                            &workflow_task,
                            &task_id,
                            &run_id,
                            &workflow_name,
                            &error,
                        )?;
                    }
                    return Ok(tool_types::ToolResult::failed(tool_request, error, None));
                }
            }
        }
        "edit" => {
            let script = input.script.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workflow draft action edit requires script",
                )
            })?;
            let edited = draft_store.edit_script(
                &input.draft_id,
                script,
                config.workflows.max_concurrent_agents,
            )?;
            WorkflowDraftActionOutput {
                status: "edited".to_string(),
                action: "edit".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name: edited.name,
                saved_path: None,
                task_id: None,
                run_id: None,
                script_path: Some(edited.script_path),
            }
        }
        "save" => {
            let workflow_dir = match input.scope.as_deref().unwrap_or("project") {
                "project" => cwd.join(".orca").join("workflows"),
                "user" => dirs::home_dir()
                    .unwrap_or_else(|| cwd.to_path_buf())
                    .join(".orca")
                    .join("workflows"),
                other => {
                    return Ok(tool_types::ToolResult::invalid_input(
                        tool_request,
                        format!("unsupported workflow draft save scope: {other}"),
                    ));
                }
            };
            let saved_path = draft_store.save_reusable(
                &input.draft_id,
                &workflow_dir,
                input.save_as.as_deref(),
            )?;
            WorkflowDraftActionOutput {
                status: "saved".to_string(),
                action: "save".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name: draft.name,
                saved_path: Some(saved_path.display().to_string()),
                task_id: None,
                run_id: None,
                script_path: Some(draft.script_path),
            }
        }
        "cancel" => {
            draft_store.cancel(&input.draft_id)?;
            WorkflowDraftActionOutput {
                status: "cancelled".to_string(),
                action: "cancel".to_string(),
                draft_id: input.draft_id,
                workflow_name: draft.name,
                saved_path: None,
                task_id: None,
                run_id: None,
                script_path: None,
            }
        }
        other => {
            return Ok(tool_types::ToolResult::invalid_input(
                tool_request,
                format!("unsupported workflow draft action: {other}"),
            ));
        }
    };

    let output = serde_json::to_string(&output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(tool_types::ToolResult::completed(
        tool_request,
        output,
        false,
    ))
}

pub(crate) fn observe_background_workflows(
    wait: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    task_registry: &TaskRegistry,
    cancel: &CancelToken,
    workflow_ingress: Option<&dyn RuntimeWorkflowLifecycleIngress>,
) -> io::Result<()> {
    if !wait {
        for workflow in background_workflows.drain(..) {
            workflow.join_silently();
        }
        return Ok(());
    }

    for workflow in background_workflows.drain(..) {
        let BackgroundWorkflowRun {
            task_id,
            run_id,
            workflow_name,
            task,
            handle,
            tool_use_id: _,
            ingress_receipt,
        } = workflow;
        let mut stop_requested = false;
        let mut stop_request_error = None;
        while !handle.is_finished() {
            if cancel.is_cancelled() && !stop_requested {
                stop_requested = true;
                if let Err(error) = task_registry.request_stop(&task_id) {
                    stop_request_error = Some(io::Error::other(error));
                }
            }
            thread::sleep(WORKFLOW_COMPLETION_POLL_INTERVAL);
        }
        let completion = (|| -> io::Result<()> {
            match handle.join() {
                Ok(Ok(result)) => {
                    let outcome = if stop_requested || cancel.is_cancelled() {
                        RuntimeWorkflowOutcome::Cancelled {
                            reason: DisplayText::new("Workflow cancelled"),
                        }
                    } else {
                        RuntimeWorkflowOutcome::Completed {
                            status_line: DisplayText::new(result.status_line.clone()),
                        }
                    };
                    commit_workflow_finished(workflow_ingress, ingress_receipt, outcome)?;
                    emit_workflow_completed(
                        events,
                        sink,
                        &task,
                        &task_id,
                        &run_id,
                        &workflow_name,
                        &result.status_line,
                    )?;
                }
                Ok(Err(error)) => {
                    let outcome = if stop_requested || cancel.is_cancelled() {
                        RuntimeWorkflowOutcome::Cancelled {
                            reason: DisplayText::new(error.to_string()),
                        }
                    } else {
                        RuntimeWorkflowOutcome::Failed {
                            error: DisplayText::new(error.to_string()),
                        }
                    };
                    commit_workflow_finished(workflow_ingress, ingress_receipt, outcome)?;
                    emit_workflow_failed(
                        events,
                        sink,
                        &task,
                        &task_id,
                        &run_id,
                        &workflow_name,
                        &error.to_string(),
                    )?;
                }
                Err(_) => {
                    let outcome = if stop_requested || cancel.is_cancelled() {
                        RuntimeWorkflowOutcome::Cancelled {
                            reason: DisplayText::new("workflow thread panicked after cancellation"),
                        }
                    } else {
                        RuntimeWorkflowOutcome::Failed {
                            error: DisplayText::new("workflow thread panicked"),
                        }
                    };
                    commit_workflow_finished(workflow_ingress, ingress_receipt, outcome)?;
                    emit_workflow_failed(
                        events,
                        sink,
                        &task,
                        &task_id,
                        &run_id,
                        &workflow_name,
                        "workflow thread panicked",
                    )?;
                }
            }
            Ok(())
        })();
        completion?;
        if let Some(error) = stop_request_error {
            return Err(error);
        }
    }

    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDraftActionInput {
    draft_id: String,
    action: String,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    save_as: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    token_budget: Option<u64>,
}

fn parse_workflow_draft_action_input(
    tool_request: &tool_types::ToolRequest,
) -> io::Result<WorkflowDraftActionInput> {
    let raw_arguments = tool_request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn parse_workflow_input(tool_request: &tool_types::ToolRequest) -> io::Result<WorkflowInput> {
    let raw_arguments = tool_request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::model::ModelSelection;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
    use orca_core::workflow_types::WorkflowOutput;

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::runtime_surface::{
        RuntimeWorkflowFinished, RuntimeWorkflowIngressReceipt, RuntimeWorkflowLifecycleIngress,
        RuntimeWorkflowOutcome, RuntimeWorkflowStarted, SurfaceTaskFence, SurfaceWorkflowFence,
        TaskRevision, WorkflowRevision,
    };
    use crate::tasks::TaskRegistry;
    use crate::workflow::host::WorkflowHost;
    use crate::workflow::runner::{
        SharedEventBuffer, WorkflowBackgroundLaunch, WorkflowLaunchResult,
    };

    use super::{
        BackgroundWorkflowRun, WorkflowDraftStore, execute_workflow_draft_action_tool,
        execute_workflow_tool, observe_background_workflows,
    };

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum RecordedWorkflowIngress {
        Started(RuntimeWorkflowStarted),
        Finished(RuntimeWorkflowFinished),
    }

    #[derive(Debug, Default)]
    struct RecordingWorkflowIngress {
        events: Mutex<Vec<RecordedWorkflowIngress>>,
    }

    impl RecordingWorkflowIngress {
        fn events(&self) -> Vec<RecordedWorkflowIngress> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl RuntimeWorkflowLifecycleIngress for RecordingWorkflowIngress {
        fn commit_started(
            &self,
            started: &RuntimeWorkflowStarted,
        ) -> io::Result<RuntimeWorkflowIngressReceipt> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(RecordedWorkflowIngress::Started(started.clone()));
            Ok(RuntimeWorkflowIngressReceipt {
                workflow: SurfaceWorkflowFence {
                    workflow_run_id: started.workflow_run_id.clone(),
                    workflow_revision: WorkflowRevision::try_new(1).unwrap(),
                    parent: None,
                },
                task: SurfaceTaskFence {
                    task_id: started.task_id.clone(),
                    task_revision: TaskRevision::try_new(1).unwrap(),
                    background_owner: None,
                },
                tool_call_id: started.tool_call_id.clone(),
            })
        }

        fn commit_finished(&self, finished: &RuntimeWorkflowFinished) -> io::Result<()> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(RecordedWorkflowIngress::Finished(finished.clone()));
            Ok(())
        }
    }

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn tool_request(id: &str, name: ToolName, raw_arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    fn unused_child_executor(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, SharedEventBuffer>,
        _cost: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("startup failure test must not execute child agents")
    }

    fn startup_failure_script() -> &'static str {
        r#"
throw new Error("startup boom");
export const meta = {
  name: "bad-workflow",
  description: "Fails on load",
  phases: [{ name: "main", tasks: [{ prompt: "noop" }] }]
};
"#
    }

    #[test]
    fn workflow_tool_reports_immediate_startup_failure() {
        let _guard = crate::history::lock_test_env();
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("session-workflow-immediate-failure".to_string());
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::<BackgroundWorkflowRun>::new();
        let request = tool_request(
            "workflow",
            ToolName::Workflow,
            serde_json::json!({ "script": startup_failure_script() }),
        );

        let result = execute_workflow_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
            true,
            None,
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("startup boom")),
            "expected startup failure details, got {result:?}"
        );
        assert!(background_workflows.is_empty());
    }

    #[test]
    fn workflow_draft_action_run_reports_immediate_startup_failure() {
        let _guard = crate::history::lock_test_env();
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("session-draft-immediate-failure".to_string());
        let session_dir = registry.workflow_session_dir(temp.path()).unwrap();
        let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
        let draft = draft_store
            .create_from_script(
                registry.session_id(),
                temp.path(),
                startup_failure_script(),
                config.workflows.max_concurrent_agents,
            )
            .unwrap();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::<BackgroundWorkflowRun>::new();
        let request = tool_request(
            "run",
            ToolName::WorkflowDraftAction,
            serde_json::json!({ "draftId": draft.draft_id, "action": "run" }),
        );

        let result = execute_workflow_draft_action_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
            true,
            None,
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("startup boom")),
            "expected startup failure details, got {result:?}"
        );
        assert!(background_workflows.is_empty());
    }

    #[test]
    fn typed_workflow_ingress_records_started_then_completed() {
        let _guard = crate::history::lock_test_env();
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("typed-workflow-completed".to_string());
        let mut events = EventFactory::new("typed-workflow-completed".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::new();
        let ingress = RecordingWorkflowIngress::default();
        let request = tool_request(
            "workflow-completed",
            ToolName::Workflow,
            serde_json::json!({
                "script": "export const meta = { name: 'typed-completed', description: 'typed completed', phases: ['main'] }; export default 'done';"
            }),
        );

        let result = execute_workflow_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
            true,
            Some(&ingress),
        )
        .unwrap();
        observe_background_workflows(
            true,
            &mut events,
            &mut sink,
            &mut background_workflows,
            &registry,
            &orca_core::cancel::CancelToken::new(),
            Some(&ingress),
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Completed);
        let recorded = ingress.events();
        assert!(matches!(
            recorded.as_slice(),
            [
                RecordedWorkflowIngress::Started(_),
                RecordedWorkflowIngress::Finished(RuntimeWorkflowFinished {
                    outcome: RuntimeWorkflowOutcome::Completed { .. },
                    ..
                })
            ]
        ));
    }

    #[test]
    fn typed_workflow_ingress_records_started_then_failed() {
        let _guard = crate::history::lock_test_env();
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("typed-workflow-failed".to_string());
        let mut events = EventFactory::new("typed-workflow-failed".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::new();
        let ingress = RecordingWorkflowIngress::default();
        let request = tool_request(
            "workflow-failed",
            ToolName::Workflow,
            serde_json::json!({ "script": startup_failure_script() }),
        );

        let result = execute_workflow_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
            true,
            Some(&ingress),
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(matches!(
            ingress.events().as_slice(),
            [
                RecordedWorkflowIngress::Started(_),
                RecordedWorkflowIngress::Finished(RuntimeWorkflowFinished {
                    outcome: RuntimeWorkflowOutcome::Failed { .. },
                    ..
                })
            ]
        ));
    }

    #[test]
    fn typed_workflow_wait_cancels_registry_before_join_and_records_cancelled() {
        let registry = TaskRegistry::new("typed-workflow-cancelled".to_string());
        let task = registry.create_workflow(
            "run-cancelled".to_string(),
            "typed-cancelled".to_string(),
            "typed cancelled".to_string(),
            1,
        );
        registry.mark_running(&task.id).unwrap();
        let worker_registry = registry.clone();
        let worker_task_id = task.id.clone();
        let worker = thread::spawn(move || {
            while !worker_registry
                .get(&worker_task_id)
                .expect("workflow task")
                .control
                .cancel
                .is_cancelled()
            {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(WorkflowLaunchResult {
                task_id: worker_task_id.clone(),
                output: WorkflowOutput {
                    status: "cancelled".to_string(),
                    task_id: worker_task_id,
                    task_type: Some("local_workflow".to_string()),
                    workflow_name: Some("typed-cancelled".to_string()),
                    run_id: Some("run-cancelled".to_string()),
                    summary: Some("cancelled".to_string()),
                    transcript_dir: None,
                    script_path: None,
                    session_url: None,
                    budget: None,
                },
                summary: "cancelled".to_string(),
                status_line: "cancelled".to_string(),
            })
        });
        let launch = WorkflowBackgroundLaunch::for_test(
            task.id.clone(),
            "run-cancelled".to_string(),
            "typed-cancelled".to_string(),
            vec!["main".to_string()],
            worker,
        );
        let ingress = Arc::new(RecordingWorkflowIngress::default());
        let started = super::commit_workflow_started(
            Some(ingress.as_ref()),
            &launch,
            "workflow-cancelled",
            &registry,
        )
        .unwrap();
        let mut background_workflows = vec![BackgroundWorkflowRun::new_with_ingress(
            launch,
            Some("workflow-cancelled".to_string()),
            started,
        )];
        let cancel = orca_core::cancel::CancelToken::new();
        cancel.cancel();
        let mut events = EventFactory::new("typed-workflow-cancelled".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);

        observe_background_workflows(
            true,
            &mut events,
            &mut sink,
            &mut background_workflows,
            &registry,
            &cancel,
            Some(ingress.as_ref()),
        )
        .unwrap();

        assert!(
            registry
                .get(&task.id)
                .unwrap()
                .control
                .cancel
                .is_cancelled()
        );
        assert!(matches!(
            ingress.events().as_slice(),
            [
                RecordedWorkflowIngress::Started(_),
                RecordedWorkflowIngress::Finished(RuntimeWorkflowFinished {
                    outcome: RuntimeWorkflowOutcome::Cancelled { .. },
                    ..
                })
            ]
        ));
    }

    #[test]
    fn typed_workflow_stop_failure_still_joins_worker_before_returning() {
        let registry = TaskRegistry::new("typed-workflow-stop-failure".to_string());
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            finished_tx.send(()).unwrap();
            Ok(WorkflowLaunchResult {
                task_id: "missing-task".to_string(),
                output: WorkflowOutput {
                    status: "completed".to_string(),
                    task_id: "missing-task".to_string(),
                    task_type: Some("local_workflow".to_string()),
                    workflow_name: Some("typed-stop-failure".to_string()),
                    run_id: Some("run-stop-failure".to_string()),
                    summary: Some("completed".to_string()),
                    transcript_dir: None,
                    script_path: None,
                    session_url: None,
                    budget: None,
                },
                summary: "completed".to_string(),
                status_line: "completed".to_string(),
            })
        });
        let launch = WorkflowBackgroundLaunch::for_test(
            "missing-task".to_string(),
            "run-stop-failure".to_string(),
            "typed-stop-failure".to_string(),
            vec!["main".to_string()],
            worker,
        );
        let mut background_workflows = vec![BackgroundWorkflowRun::new(launch, None)];
        let cancel = orca_core::cancel::CancelToken::new();
        cancel.cancel();
        let mut events = EventFactory::new("typed-workflow-stop-failure".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);

        let error = observe_background_workflows(
            true,
            &mut events,
            &mut sink,
            &mut background_workflows,
            &registry,
            &cancel,
            None,
        )
        .expect_err("missing workflow task must report its stop failure");

        assert!(error.to_string().contains("missing-task"));
        finished_rx
            .try_recv()
            .expect("workflow worker must be joined before the stop error returns");
        assert!(background_workflows.is_empty());
    }

    #[test]
    fn typed_workflow_wait_false_rejects_before_creating_task() {
        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("typed-workflow-no-wait".to_string());
        let mut events = EventFactory::new("typed-workflow-no-wait".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::new();
        let ingress = RecordingWorkflowIngress::default();
        let request = tool_request(
            "workflow-no-wait",
            ToolName::Workflow,
            serde_json::json!({ "script": "external side effect must never start" }),
        );

        let result = execute_workflow_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
            false,
            Some(&ingress),
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(registry.list().is_empty());
        assert!(background_workflows.is_empty());
        assert!(ingress.events().is_empty());
    }
}
