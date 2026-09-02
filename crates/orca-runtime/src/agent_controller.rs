use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use orca_core::agent_event::{AgentActivity, AgentEvent, AgentEventEnvelope, AgentUsage};
use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Message;
use orca_core::event_schema::{EventEnvelope, EventType, RunStatus};
use orca_core::event_sink::EventObserver;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;

use crate::agent_registry::AgentRegistry;
use crate::lifecycle::RuntimePermissionRequestHandler;
use crate::runtime_host::{
    HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle,
};

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

pub(crate) struct AgentLaunchRequest {
    pub(crate) batch_id: String,
    pub(crate) batch_size: u32,
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
    registry: Arc<AgentRegistry>,
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
        registry: Arc<AgentRegistry>,
        root_thread_id: String,
        parent_thread_id: String,
        depth: u32,
    ) -> Self {
        Self {
            host,
            registry,
            root_thread_id,
            parent_thread_id,
            depth,
        }
    }

    pub(crate) fn launch(&self, request: AgentLaunchRequest) -> io::Result<AgentLaunchResult> {
        if self.depth >= request.config.subagents.max_depth {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "agent max depth {} reached",
                    request.config.subagents.max_depth
                ),
            ));
        }

        let mut child_config = request.config;
        child_config.history_mode = HistoryMode::Record;
        child_config.prompt.clear();
        if request.model.is_some() {
            child_config.model = ModelSelection::parse(request.model).map_err(io::Error::other)?;
        }

        let child = self
            .host
            .start_agent_thread(
                &self.parent_thread_id,
                &self.root_thread_id,
                self.depth.saturating_add(1),
                request.subagent_type.clone(),
                child_config.clone(),
                request.description.clone(),
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
        let thread_id = child.thread_id().to_string();
        if let Err(error) = child.mutate(
            crate::runtime_host::RuntimeThreadMutation::AddPinnedContext(format!(
                "You are a delegated child agent. Work only on this delegated task and keep all \
                 tool activity in this thread.{}",
                request.subagent_type.system_prompt_suffix()
            )),
        ) {
            let _ = child.shutdown();
            return Err(io::Error::other(error.to_string()));
        }
        let attempt_id = uuid::Uuid::now_v7().to_string();
        let publisher = Arc::new(AgentEventPublisher::new(
            Arc::clone(&self.registry),
            self.root_thread_id.clone(),
            request.agent_id.clone(),
            thread_id.clone(),
            attempt_id,
        ));
        publisher.publish(AgentEvent::Spawned {
            batch_id: request.batch_id,
            batch_size: request.batch_size,
            parent_thread_id: self.parent_thread_id.clone(),
            description: request.description.clone(),
        })?;
        publisher.publish(AgentEvent::Activity {
            activity: AgentActivity::Starting,
            turn: None,
            usage: None,
        })?;

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
                let _ = publisher.publish(AgentEvent::Failed {
                    reason: error.to_string(),
                    usage: AgentUsage::default(),
                });
                return Err(io::Error::other(error.to_string()));
            }
        };

        if request.mode == AgentRunMode::Async {
            let completion_child = child.clone();
            std::thread::Builder::new()
                .name(format!("orca-agent-wait-{}", request.agent_id))
                .spawn(move || {
                    settle_operation(&completion_child, operation, &publisher, None);
                })
                .map_err(|error| io::Error::other(format!("failed to watch agent: {error}")))?;
            return Ok(AgentLaunchResult {
                thread_id,
                status: RunStatus::Success,
                final_message: None,
                error: None,
                usage: Default::default(),
                running: true,
            });
        }

        Ok(settle_operation(
            &child,
            operation,
            &publisher,
            Some(&request.cancel),
        ))
    }
}

struct AgentEventPublisher {
    registry: Arc<AgentRegistry>,
    root_thread_id: String,
    agent_id: String,
    thread_id: String,
    attempt_id: String,
    next_sequence: Mutex<u64>,
    usage: Mutex<AgentUsage>,
}

impl AgentEventPublisher {
    fn new(
        registry: Arc<AgentRegistry>,
        root_thread_id: String,
        agent_id: String,
        thread_id: String,
        attempt_id: String,
    ) -> Self {
        Self {
            registry,
            root_thread_id,
            agent_id,
            thread_id,
            attempt_id,
            next_sequence: Mutex::new(1),
            usage: Mutex::new(AgentUsage::default()),
        }
    }

    fn publish(&self, event: AgentEvent) -> io::Result<()> {
        let mut next_sequence = self
            .next_sequence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let envelope = AgentEventEnvelope::new(
            uuid::Uuid::now_v7().to_string(),
            self.root_thread_id.clone(),
            self.agent_id.clone(),
            self.thread_id.clone(),
            self.attempt_id.clone(),
            *next_sequence,
            chrono::Utc::now().timestamp_millis(),
            event,
        );
        self.registry.append(envelope)?;
        *next_sequence = next_sequence.saturating_add(1);
        Ok(())
    }
}

impl EventObserver for AgentEventPublisher {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        let text = |key: &str| {
            event
                .payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        match event.event_type {
            EventType::TurnStarted => self.publish(AgentEvent::Activity {
                activity: AgentActivity::Thinking,
                turn: event
                    .payload
                    .get("turn")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|turn| u32::try_from(turn).ok()),
                usage: None,
            }),
            EventType::ToolCallRequested => self.publish(AgentEvent::Activity {
                activity: AgentActivity::Tool {
                    name: text("name").unwrap_or_else(|| "tool".to_string()),
                    target: text("target"),
                },
                turn: None,
                usage: None,
            }),
            EventType::AssistantMessageDelta => text("text").map_or(Ok(()), |text| {
                self.publish(AgentEvent::OutputDelta { text })
            }),
            EventType::ApprovalRequested => self.publish(AgentEvent::PermissionRequested {
                description: text("description")
                    .unwrap_or_else(|| "tool approval required".to_string()),
            }),
            EventType::UsageUpdated => {
                let usage = AgentUsage {
                    input_tokens: event
                        .payload
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    output_tokens: event
                        .payload
                        .get("output_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    cache_tokens: event
                        .payload
                        .get("cache_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    cost_micro_usd: crate::cost::usd_to_micros(
                        event
                            .payload
                            .get("estimated_cost_usd")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or_default(),
                    ),
                };
                *self.usage.lock().unwrap_or_else(PoisonError::into_inner) = usage.clone();
                self.publish(AgentEvent::Activity {
                    activity: AgentActivity::Thinking,
                    turn: None,
                    usage: Some(usage),
                })
            }
            _ => Ok(()),
        }
    }
}

fn settle_operation(
    child: &RuntimeThreadHandle,
    operation: OperationHandle,
    publisher: &AgentEventPublisher,
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
    let agent_usage = AgentUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_tokens: usage.cache_tokens,
        cost_micro_usd: crate::cost::usd_to_micros(usage.estimated_cost_usd),
    };
    let final_message = snapshot.as_ref().and_then(|snapshot| {
        snapshot.messages().iter().rev().find_map(|message| {
            let Message::Assistant { content, .. } = message else {
                return None;
            };
            content.clone()
        })
    });

    let (status, error, event) = match terminal.outcome() {
        OperationOutcome::Completed(RunStatus::Success) => (
            RunStatus::Success,
            None,
            AgentEvent::Completed {
                result: final_message.clone(),
                usage: agent_usage.clone(),
            },
        ),
        OperationOutcome::Completed(RunStatus::Cancelled) => (
            RunStatus::Cancelled,
            Some("agent cancelled".to_string()),
            AgentEvent::Cancelled {
                reason: "agent cancelled".to_string(),
                usage: agent_usage.clone(),
            },
        ),
        OperationOutcome::Completed(RunStatus::ApprovalRequired) => (
            RunStatus::ApprovalRequired,
            Some("agent requires permission".to_string()),
            AgentEvent::PermissionRequested {
                description: "agent requires permission".to_string(),
            },
        ),
        OperationOutcome::Completed(status) => {
            let reason = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.completion_error())
                .unwrap_or_else(|| status.as_str())
                .to_string();
            (
                *status,
                Some(reason.clone()),
                AgentEvent::Failed {
                    reason,
                    usage: agent_usage.clone(),
                },
            )
        }
        OperationOutcome::Stopped(terminal) => {
            let reason = format!("agent stopped: {terminal:?}");
            (
                RunStatus::Failed,
                Some(reason.clone()),
                AgentEvent::Failed {
                    reason,
                    usage: agent_usage.clone(),
                },
            )
        }
        OperationOutcome::Backgrounded { task_id } => {
            let _ = task_id;
            (
                RunStatus::Success,
                None,
                AgentEvent::Activity {
                    activity: AgentActivity::Thinking,
                    turn: None,
                    usage: Some(agent_usage.clone()),
                },
            )
        }
        OperationOutcome::ExecutionFailed { message, .. }
        | OperationOutcome::Panicked { message } => (
            RunStatus::Failed,
            Some(message.clone()),
            AgentEvent::Failed {
                reason: message.clone(),
                usage: agent_usage.clone(),
            },
        ),
    };
    let _ = publisher.publish(event);
    AgentLaunchResult {
        thread_id: child.thread_id().to_string(),
        status,
        final_message,
        error,
        usage,
        running: false,
    }
}
