use std::io;
use std::sync::Arc;
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Message;
use orca_core::event_schema::{EventEnvelope, EventType, RunStatus};
use orca_core::event_sink::EventObserver;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;

use crate::agent_child::ChildAgentActivity;
use crate::child_agent_types::{
    ChildAgentActivityEmitter, ChildAgentActivityPublisher, SubagentActivityPayload,
};
use crate::lifecycle::RuntimePermissionRequestHandler;
use crate::runtime_host::{
    HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle,
};
use crate::runtime_surface::DisplayText;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentRunMode {
    Sync,
    Async,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentThreadPolicy {
    pub(crate) subagent_type: SubagentType,
    pub(crate) depth: u32,
}

/// Optional bridge that mirrors a sync child's activity onto the parent
/// thread's runtime surface as a `SurfaceSubagent`, so the surface (not the
/// registry) is the live UI source. Only supplied for sync children, whose
/// generation-owned ingress stays valid for the duration of the parent turn;
/// async children outlive that generation and remain on the registry rail.
#[derive(Clone)]
pub(crate) struct AgentSurfaceActivity {
    pub(crate) emitter: Arc<ChildAgentActivityEmitter>,
    pub(crate) description: String,
    pub(crate) batch_id: String,
    pub(crate) batch_size: u32,
}

impl std::fmt::Debug for AgentSurfaceActivity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSurfaceActivity")
            .field("description", &self.description)
            .field("batch_id", &self.batch_id)
            .field("batch_size", &self.batch_size)
            .finish_non_exhaustive()
    }
}

impl AgentSurfaceActivity {
    pub(crate) fn publish_terminal(
        &self,
        status: RunStatus,
        output: Option<&str>,
        error: Option<&str>,
        usage: orca_core::cost_types::UsageTotals,
    ) -> io::Result<()> {
        publish_surface_terminal(self, status, output, error, usage)
    }
}

pub(crate) struct AgentLaunchRequest {
    pub(crate) agent_id: String,
    pub(crate) description: String,
    pub(crate) prompt: String,
    pub(crate) model: Option<String>,
    pub(crate) subagent_type: SubagentType,
    pub(crate) mode: AgentRunMode,
    pub(crate) config: RunConfig,
    pub(crate) cancel: CancelToken,
    pub(crate) approval_handler:
        Option<Arc<dyn crate::lifecycle::RuntimeApprovalHandler + Send + Sync>>,
    pub(crate) permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    pub(crate) surface_activity: Option<AgentSurfaceActivity>,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentLaunchResult {
    pub(crate) thread_id: String,
    pub(crate) status: RunStatus,
    pub(crate) final_message: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) usage: orca_core::cost_types::UsageTotals,
    pub(crate) running: bool,
}

#[derive(Clone)]
pub(crate) struct AgentController {
    host: RuntimeHostHandle,
    root_thread_id: String,
    parent_thread_id: String,
    depth: u32,
}

impl std::fmt::Debug for AgentController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentController")
            .field("root_thread_id", &self.root_thread_id)
            .field("parent_thread_id", &self.parent_thread_id)
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

impl AgentController {
    pub(crate) fn new(
        host: RuntimeHostHandle,
        root_thread_id: String,
        parent_thread_id: String,
        depth: u32,
    ) -> Self {
        Self {
            host,
            root_thread_id,
            parent_thread_id,
            depth,
        }
    }

    pub(crate) fn launch(&self, request: AgentLaunchRequest) -> io::Result<AgentLaunchResult> {
        let surface_activity = request.surface_activity;
        if let Some(activity) = surface_activity.as_ref() {
            activity
                .emitter
                .publish_payload(SubagentActivityPayload::Started {
                    description: DisplayText::new(&activity.description),
                    batch_id: activity.batch_id.clone(),
                    batch_size: activity.batch_size,
                })?;
        }

        if self.depth >= request.config.subagents.max_depth {
            let message = format!(
                "agent max depth {} reached",
                request.config.subagents.max_depth
            );
            let message = failure_after_started_message(surface_activity.as_ref(), &message);
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
        }

        let mut child_config = request.config;
        child_config.history_mode = HistoryMode::Record;
        child_config.prompt.clear();
        if request.model.is_some() {
            child_config.model = match ModelSelection::parse(request.model) {
                Ok(model) => model,
                Err(error) => {
                    let message = failure_after_started_message(
                        surface_activity.as_ref(),
                        &format!("invalid delegated child model: {error}"),
                    );
                    return Err(io::Error::other(message));
                }
            };
        }

        let child = match self.host.start_agent_thread(
            &self.parent_thread_id,
            &self.root_thread_id,
            self.depth.saturating_add(1),
            request.subagent_type.clone(),
            child_config.clone(),
            request.description.clone(),
        ) {
            Ok(child) => child,
            Err(error) => {
                let message =
                    failure_after_started_message(surface_activity.as_ref(), &error.to_string());
                return Err(io::Error::other(message));
            }
        };
        let thread_id = child.thread_id().to_string();
        if let Err(error) = child.mutate(
            crate::runtime_host::RuntimeThreadMutation::AddPinnedContext(format!(
                "You are a delegated child agent. Work only on this delegated task and keep all \
                 tool activity in this thread.{}",
                request.subagent_type.system_prompt_suffix()
            )),
        ) {
            let _ = child.shutdown();
            let message =
                failure_after_started_message(surface_activity.as_ref(), &error.to_string());
            return Err(io::Error::other(message));
        }
        let publisher = Arc::new(AgentEventPublisher::new(surface_activity));
        if let Err(error) = publisher.publish_surface_bound(&thread_id) {
            let terminal_error = publisher.publish_surface_terminal(
                RunStatus::Failed,
                None,
                Some(error.to_string()),
                Default::default(),
            );
            let _ = child.shutdown();
            return Err(match terminal_error {
                Ok(()) => error,
                Err(terminal_error) => io::Error::other(format!(
                    "{error}; surface terminal commit failed: {terminal_error}; Inspect external state before retrying"
                )),
            });
        }
        let mut turn = HostedTurnRequest::new(request.prompt)
            .with_task_description(request.description)
            .with_event_observer(publisher.clone());
        if let Some(handler) = request.approval_handler {
            turn = turn.with_approval_handler(handler);
        }
        if let Some(handler) = request.permission_handler {
            turn = turn.with_permission_handler(handler);
        }
        let operation = match child.start_turn(turn, io::sink()) {
            Ok(operation) => operation,
            Err(error) => {
                let terminal_error = publisher.publish_surface_terminal(
                    RunStatus::Failed,
                    None,
                    Some(error.to_string()),
                    Default::default(),
                );
                let _ = child.shutdown();
                return Err(match terminal_error {
                    Ok(()) => io::Error::other(error.to_string()),
                    Err(terminal_error) => io::Error::other(format!(
                        "{error}; surface terminal commit failed: {terminal_error}; Inspect external state before retrying"
                    )),
                });
            }
        };

        if request.mode == AgentRunMode::Async {
            let completion_child = child.clone();
            let watcher_publisher = publisher.clone();
            if let Err(error) = std::thread::Builder::new()
                .name(format!("orca-agent-wait-{}", request.agent_id))
                .spawn(move || {
                    let result = settle_operation(&completion_child, operation, None);
                    let _ = watcher_publisher.publish_surface_terminal(
                        result.status,
                        result.final_message.clone(),
                        result.error.clone(),
                        result.usage,
                    );
                })
            {
                let reason = format!("failed to watch agent: {error}");
                let _ = child.interrupt_active();
                let terminal_error = publisher.publish_surface_terminal(
                    RunStatus::Failed,
                    None,
                    Some(reason.clone()),
                    Default::default(),
                );
                let _ = child.shutdown();
                return Err(match terminal_error {
                    Ok(()) => io::Error::other(reason),
                    Err(terminal_error) => io::Error::other(format!(
                        "{reason}; surface terminal commit failed: {terminal_error}; Inspect external state before retrying"
                    )),
                });
            }
            return Ok(AgentLaunchResult {
                thread_id,
                status: RunStatus::Success,
                final_message: None,
                error: None,
                usage: Default::default(),
                running: true,
            });
        }

        Ok(settle_operation(&child, operation, Some(&request.cancel)))
    }
}

fn failure_after_started_message(
    surface_activity: Option<&AgentSurfaceActivity>,
    message: &str,
) -> String {
    let mut message = message.to_string();
    if let Some(activity) = surface_activity
        && let Err(error) = publish_surface_terminal(
            activity,
            RunStatus::Failed,
            None,
            Some(&message),
            Default::default(),
        )
    {
        message.push_str(&format!(
            "; surface terminal commit failed: {error}; Inspect external state before retrying"
        ));
    }
    message
}

struct AgentEventPublisher {
    surface_activity: Option<AgentSurfacePublisher>,
}

impl AgentEventPublisher {
    fn new(surface_activity: Option<AgentSurfaceActivity>) -> Self {
        Self {
            surface_activity: surface_activity.map(AgentSurfacePublisher::new),
        }
    }

    fn publish_surface_terminal(
        &self,
        status: RunStatus,
        output: Option<String>,
        error: Option<String>,
        usage: orca_core::cost_types::UsageTotals,
    ) -> io::Result<()> {
        self.surface_activity.as_ref().map_or(Ok(()), |publisher| {
            publisher.completed(status, output.as_deref(), error.as_deref(), usage)
        })
    }

    fn publish_surface_bound(&self, thread_id: &str) -> io::Result<()> {
        self.surface_activity
            .as_ref()
            .map_or(Ok(()), |publisher| publisher.child_thread_bound(thread_id))
    }
}

impl EventObserver for AgentEventPublisher {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        self.surface_activity
            .as_ref()
            .map_or(Ok(()), |surface_activity| surface_activity.observe(event))
    }
}

struct AgentSurfacePublisher {
    emitter: Arc<ChildAgentActivityEmitter>,
}

impl AgentSurfacePublisher {
    fn new(activity: AgentSurfaceActivity) -> Self {
        Self {
            emitter: activity.emitter,
        }
    }

    fn child_thread_bound(&self, thread_id: &str) -> io::Result<()> {
        let uuid = uuid::Uuid::parse_str(thread_id).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("child runtime returned invalid thread id: {error}"),
            )
        })?;
        let thread_id =
            crate::runtime_surface::SurfaceThreadId::try_from_bytes(*uuid.as_bytes())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        self.emitter
            .publish_payload(SubagentActivityPayload::ChildThreadBound { thread_id })
    }

    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        let required_text = |field: &str| {
            event.payload[field]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "threaded child event {:?} is missing string field '{field}'",
                            event.event_type
                        ),
                    )
                })
        };
        let activity = match event.event_type {
            EventType::TurnStarted => Some(ChildAgentActivity::TurnStarted {
                turn: event.payload["turn"].as_u64().unwrap_or_default() as u32,
            }),
            EventType::AssistantReasoningDelta | EventType::AssistantMessageDelta => {
                Some(ChildAgentActivity::Streaming)
            }
            EventType::ToolCallRequested => Some(ChildAgentActivity::ToolStarted {
                call_id: required_text("id")?,
                name: required_text("name")?,
                target: event.payload["target"].as_str().map(str::to_string),
            }),
            EventType::ToolCallCompleted => Some(ChildAgentActivity::ToolCompleted {
                call_id: required_text("id")?,
                name: required_text("name")?,
                status: match event.payload["status"].as_str() {
                    Some("completed") => RunStatus::Success,
                    Some("cancelled") => RunStatus::Cancelled,
                    _ => RunStatus::Failed,
                },
            }),
            EventType::UsageUpdated => Some(ChildAgentActivity::Usage(
                orca_core::cost_types::UsageTotals {
                    input_tokens: event.payload["input_tokens"].as_u64().unwrap_or_default(),
                    output_tokens: event.payload["output_tokens"].as_u64().unwrap_or_default(),
                    cache_tokens: event.payload["cache_tokens"].as_u64().unwrap_or_default(),
                    estimated_cost_usd: event.payload["estimated_cost_usd"]
                        .as_f64()
                        .unwrap_or_default(),
                },
            )),
            _ => None,
        };
        if let Some(activity) = activity {
            self.emitter.publish_activity(activity)?;
        }
        Ok(())
    }

    fn completed(
        &self,
        status: RunStatus,
        output: Option<&str>,
        error: Option<&str>,
        usage: orca_core::cost_types::UsageTotals,
    ) -> io::Result<()> {
        self.emitter
            .publish_payload(SubagentActivityPayload::Completed {
                status: match status {
                    RunStatus::Success => {
                        crate::runtime_surface::SurfaceSubagentTerminalStatus::Completed
                    }
                    RunStatus::Cancelled => {
                        crate::runtime_surface::SurfaceSubagentTerminalStatus::Cancelled
                    }
                    RunStatus::Failed
                    | RunStatus::ApprovalRequired
                    | RunStatus::VerificationFailed => {
                        crate::runtime_surface::SurfaceSubagentTerminalStatus::Failed
                    }
                },
                output: output.map(DisplayText::new),
                error: error.map(DisplayText::new),
                usage: Some(usage),
            })
    }
}

fn publish_surface_terminal(
    activity: &AgentSurfaceActivity,
    status: RunStatus,
    output: Option<&str>,
    error: Option<&str>,
    usage: orca_core::cost_types::UsageTotals,
) -> io::Result<()> {
    let terminal_status = match status {
        RunStatus::Success => crate::runtime_surface::SurfaceSubagentTerminalStatus::Completed,
        RunStatus::Cancelled => crate::runtime_surface::SurfaceSubagentTerminalStatus::Cancelled,
        RunStatus::Failed | RunStatus::ApprovalRequired | RunStatus::VerificationFailed => {
            crate::runtime_surface::SurfaceSubagentTerminalStatus::Failed
        }
    };
    activity
        .emitter
        .publish_payload(SubagentActivityPayload::Completed {
            status: terminal_status,
            output: output.map(DisplayText::new),
            error: error.map(DisplayText::new),
            usage: Some(usage),
        })
}

fn settle_operation(
    child: &RuntimeThreadHandle,
    operation: OperationHandle,
    cancel: Option<&CancelToken>,
) -> AgentLaunchResult {
    let terminal = loop {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            let _ = operation.interrupt();
        }
        if let Some(terminal) = operation.wait_timeout(Duration::from_millis(50)) {
            break terminal;
        }
    };
    let snapshot = child.snapshot().ok();
    let usage = snapshot
        .as_ref()
        .map(|snapshot| snapshot.usage_totals())
        .unwrap_or_default();
    let final_message = snapshot.as_ref().and_then(|snapshot| {
        snapshot.messages().iter().rev().find_map(|message| {
            let Message::Assistant { content, .. } = message else {
                return None;
            };
            content.clone()
        })
    });

    let cancellation_requested = cancel.is_some_and(CancelToken::is_cancelled);
    let (status, error) = if cancellation_requested
        || matches!(terminal.outcome(), OperationOutcome::Stopped(_))
    {
        (RunStatus::Cancelled, Some("agent cancelled".to_string()))
    } else {
        match terminal.outcome() {
            OperationOutcome::Completed(RunStatus::Success) => (RunStatus::Success, None),
            OperationOutcome::Completed(RunStatus::Cancelled) => {
                (RunStatus::Cancelled, Some("agent cancelled".to_string()))
            }
            OperationOutcome::Completed(RunStatus::ApprovalRequired) => (
                RunStatus::ApprovalRequired,
                Some("agent requires permission".to_string()),
            ),
            OperationOutcome::Completed(status) => {
                let reason = snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.completion_error())
                    .unwrap_or_else(|| status.as_str())
                    .to_string();
                (*status, Some(reason))
            }
            OperationOutcome::Stopped(terminal) => {
                let reason = format!("agent stopped: {terminal:?}");
                (RunStatus::Cancelled, Some(reason))
            }
            OperationOutcome::Backgrounded { task_id } => {
                let _ = task_id;
                (RunStatus::Success, None)
            }
            OperationOutcome::ExecutionFailed { message, .. }
            | OperationOutcome::Panicked { message } => (RunStatus::Failed, Some(message.clone())),
        }
    };
    AgentLaunchResult {
        thread_id: child.thread_id().to_string(),
        status,
        final_message,
        error,
        usage,
        running: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_continuation::AgentAttemptId;
    use crate::child_agent_types::SubagentActivityIdentity;
    use crate::runtime_subagent_call::RuntimeSubagentActivitySink;
    use crate::runtime_surface::{
        RuntimeSubagentActivityIngress, Sha256Digest, SurfaceSubagentId, SurfaceTaskId,
        TaskRevision,
    };
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingActivityIngress {
        events: Mutex<Vec<crate::child_agent_types::SubagentActivityEvent>>,
    }

    impl RuntimeSubagentActivityIngress for RecordingActivityIngress {
        fn owner(&self) -> crate::child_agent_types::SubagentActivityOwner {
            crate::child_agent_types::SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("threaded-sync-task").expect("task id"),
                task_revision: TaskRevision::try_new(1).expect("task revision"),
                authority_digest: Sha256Digest::new([3; 32]),
            }
        }

        fn commit_activity(
            &self,
            event: crate::child_agent_types::SubagentActivityEvent,
        ) -> io::Result<()> {
            self.events
                .lock()
                .expect("recording ingress lock")
                .push(event);
            Ok(())
        }
    }

    fn event(event_type: EventType, payload: serde_json::Value) -> EventEnvelope {
        EventEnvelope {
            version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
            run_id: "threaded-sync-child".to_string(),
            seq: 1,
            timestamp_ms: 1,
            event_type,
            payload,
        }
    }

    #[test]
    fn threaded_sync_surface_publisher_emits_ordered_child_facts_through_parent_ingress() {
        let ingress = Arc::new(RecordingActivityIngress::default());
        let activity = AgentSurfaceActivity {
            emitter: Arc::new(ChildAgentActivityEmitter::new(
                SubagentActivityIdentity {
                    task_id: SurfaceTaskId::try_new("threaded-sync-task").expect("task id"),
                    subagent_id: SurfaceSubagentId::try_new("threaded-sync-child")
                        .expect("subagent id"),
                    attempt_id: AgentAttemptId::new(),
                    turn_id: orca_core::thread_identity::TurnId::new(),
                    owner: ingress.owner(),
                },
                Arc::new(RuntimeSubagentActivitySink {
                    ingress: ingress.clone(),
                }),
            )),
            description: "inspect the threaded child".to_string(),
            batch_id: "batch-threaded".to_string(),
            batch_size: 1,
        };
        let publisher = AgentSurfacePublisher::new(activity);

        publisher
            .emitter
            .publish_payload(SubagentActivityPayload::Started {
                description: DisplayText::new("inspect the threaded child"),
                batch_id: "batch-threaded".to_string(),
                batch_size: 1,
            })
            .expect("publish child start");
        publisher
            .observe(&event(
                EventType::TurnStarted,
                serde_json::json!({ "turn": 2 }),
            ))
            .expect("publish child phase");
        publisher
            .observe(&event(
                EventType::ToolCallRequested,
                serde_json::json!({ "id": "child-tool-1", "name": "shell", "target": "pwd" }),
            ))
            .expect("publish child tool");
        publisher
            .observe(&event(
                EventType::UsageUpdated,
                serde_json::json!({
                    "input_tokens": 11,
                    "output_tokens": 13,
                    "cache_tokens": 17,
                    "estimated_cost_usd": 0.000021,
                }),
            ))
            .expect("publish child usage");
        publisher
            .completed(
                RunStatus::Success,
                Some("child completed"),
                None,
                orca_core::cost_types::UsageTotals {
                    input_tokens: 11,
                    output_tokens: 13,
                    cache_tokens: 17,
                    estimated_cost_usd: 0.000021,
                },
            )
            .expect("publish child terminal");

        let events = ingress.events.lock().expect("recorded events");
        assert_eq!(events.len(), 5);
        assert!(matches!(
            events[0].payload,
            SubagentActivityPayload::Started { .. }
        ));
        assert!(matches!(
            events[1].payload,
            SubagentActivityPayload::PhaseChanged { turn: Some(2), .. }
        ));
        assert!(matches!(
            events[2].payload,
            SubagentActivityPayload::ToolStarted { ref name, .. } if name == "shell"
        ));
        assert!(matches!(
            events[3].payload,
            SubagentActivityPayload::Usage { ref totals } if totals.input_tokens == 11
        ));
        assert!(matches!(
            events[4].payload,
            SubagentActivityPayload::Completed {
                status: crate::runtime_surface::SurfaceSubagentTerminalStatus::Completed,
                ..
            }
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.payload, SubagentActivityPayload::Completed { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.source_sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }
}
