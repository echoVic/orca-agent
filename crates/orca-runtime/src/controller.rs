use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use orca_core::conversation::Message;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink};
use orca_core::subagent_types::SubagentType;
use orca_core::thread_identity::TurnId;
#[cfg(test)]
use orca_core::tool_types;
use orca_mcp::McpElicitationHandler;
#[cfg(test)]
use orca_mcp::McpRegistry;
#[cfg(test)]
use orca_tools;

use crate::agent_common;
use crate::agent_loop::run_agent_loop;
use crate::background_turn::RuntimeTurnContinuation;
#[cfg(test)]
use crate::cost::CostTracker;
use crate::extension::ExtensionData;
#[cfg(test)]
use crate::hooks::HookRunner;
#[cfg(test)]
use crate::instructions::ProjectInstructions;
#[cfg(test)]
use crate::lifecycle::RuntimeTaskKind;
use crate::lifecycle::{
    AgentLoopContext, AgentLoopOutcome, RuntimeApprovalHandler, RuntimePermissionRequestHandler,
    RuntimeSessionLifecycle, RuntimeUserInputHandler, ThreadSteerHandle,
};
use crate::provider_stream::{RuntimeProviderSuspension, RuntimeProviderSuspensionControl};
use crate::runtime_conversation_bootstrap::AgentConversationContext;
use crate::runtime_host::{
    HeadlessInteractionCheckpoint, HeadlessOperationHandle, HeadlessSurfaceSession, RuntimeHost,
    RuntimeHostError, RuntimeThreadStartRequest,
};
use crate::runtime_surface::{
    FailureClass as SurfaceFailureClass, OperationTerminal as SurfaceOperationTerminal,
    SurfaceAllowDeny, SurfaceClientInteractionAnswer, SurfaceInteractionRequest,
    SurfaceMcpElicitationDecision, SurfacePermissionClientDecision, SurfaceUserInputDecision,
};
use crate::runtime_surface::{RuntimeProviderResponseIngress, RuntimeWorkflowLifecycleIngress};
use crate::session::{InteractiveSession, InteractiveSessionRuntimeParts};
use crate::tasks::{MainSessionTerminalUpdate, TaskRegistry};
use crate::terminal_service::TerminalService;
#[cfg(test)]
use crate::thread::RuntimeThread;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow_execution::{BackgroundWorkflowRun, observe_background_workflows};

const HOSTED_EVENT_RELAY_CAPACITY: usize = 1;
const HOSTED_EVENT_RELAY_POLL: Duration = Duration::from_millis(10);
const DEFAULT_HEADLESS_INTERACTION_TIMEOUT: Duration = Duration::from_secs(30);

pub trait HeadlessInteractionHandler: Send + Sync + 'static {
    fn handle(
        &self,
        checkpoint: &HeadlessInteractionCheckpoint,
    ) -> io::Result<SurfaceClientInteractionAnswer>;
}

impl<F> HeadlessInteractionHandler for F
where
    F: Fn(&HeadlessInteractionCheckpoint) -> io::Result<SurfaceClientInteractionAnswer>
        + Send
        + Sync
        + 'static,
{
    fn handle(
        &self,
        checkpoint: &HeadlessInteractionCheckpoint,
    ) -> io::Result<SurfaceClientInteractionAnswer> {
        self(checkpoint)
    }
}

#[derive(Clone)]
pub struct HeadlessInteractionTransport {
    handler: Arc<dyn HeadlessInteractionHandler>,
    timeout: Duration,
    callback_in_flight: Arc<AtomicBool>,
    callback_unavailable: Arc<AtomicBool>,
}

impl HeadlessInteractionTransport {
    /// Register the typed Headless callback. The runtime passes the complete
    /// request plus exact private selector and accepts only a typed answer.
    pub fn new(handler: impl HeadlessInteractionHandler) -> Self {
        Self {
            handler: Arc::new(handler),
            timeout: DEFAULT_HEADLESS_INTERACTION_TIMEOUT,
            callback_in_flight: Arc::new(AtomicBool::new(false)),
            callback_unavailable: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

struct HostedEventRelayWriter {
    tx: mpsc::SyncSender<HostedEventChunk>,
    buffer: Vec<u8>,
}

struct HostedEventChunk {
    bytes: Vec<u8>,
    ack: mpsc::SyncSender<Result<(), HostedEventRelayError>>,
}

struct PendingHeadlessInteraction {
    checkpoint: HeadlessInteractionCheckpoint,
    answer_rx: mpsc::Receiver<io::Result<SurfaceClientInteractionAnswer>>,
    deadline: std::time::Instant,
}

#[derive(Clone, Debug)]
struct HostedEventRelayError {
    kind: io::ErrorKind,
    message: String,
}

impl HostedEventRelayError {
    fn from_io(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn into_io(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }
}

impl io::Write for HostedEventRelayWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(HostedEventChunk {
                bytes: std::mem::take(&mut self.buffer),
                ack: ack_tx,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "hosted event relay disconnected")
            })?;
        ack_rx
            .recv()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "hosted event relay acknowledgement closed",
                )
            })?
            .map_err(HostedEventRelayError::into_io)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ControllerRunOptions {
    pub wait_for_background_workflows: bool,
}

impl Default for ControllerRunOptions {
    fn default() -> Self {
        Self {
            wait_for_background_workflows: true,
        }
    }
}

impl ControllerRunOptions {
    fn for_run_config(config: &RunConfig) -> Self {
        Self {
            wait_for_background_workflows: config.output_format == OutputFormat::Jsonl,
        }
    }
}

pub struct ThreadTurnExecutor<'a> {
    config: &'a RunConfig,
    session: &'a mut InteractiveSession,
    lifecycle: &'a mut RuntimeSessionLifecycle,
    thread_extensions: Option<Arc<ExtensionData>>,
    turn_extension_id: Option<String>,
}

pub struct ThreadTurnContext<'a> {
    cwd: PathBuf,
    prompt: String,
    memory_start: usize,
    parts: InteractiveSessionRuntimeParts<'a>,
}

pub struct ThreadTurnExecution<W: io::Write> {
    events: EventFactory,
    sink: EventSink<W>,
    cancel: CancelToken,
    background_workflows: Vec<BackgroundWorkflowRun>,
}

struct ThreadTurnMainSessionTask {
    registry: TaskRegistry,
    id: String,
    usage_before: UsageTotals,
    task_usage_before: UsageTotals,
}

struct PreparedThreadTurn<'a, 'session, W: io::Write> {
    config: &'a RunConfig,
    lifecycle: &'a mut RuntimeSessionLifecycle,
    request: &'a ThreadTurnRequest,
    context: ThreadTurnContext<'session>,
    cancel: &'a CancelToken,
    events: &'a mut EventFactory,
    sink: &'a mut EventSink<W>,
    background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    thread_extensions: Option<Arc<ExtensionData>>,
    turn_extension_id: Option<String>,
}

struct ThreadTurnCompletion {
    cwd: PathBuf,
    memory_start: usize,
    status: RunStatus,
    end_reason: crate::lifecycle::TurnEndReason,
    error: Option<String>,
    usage: UsageTotals,
    main_session_task: Option<ThreadTurnMainSessionTask>,
    background_workflows: RuntimeBackgroundWorkflows,
    /// Typed operation terminal; the authoritative fact when present (budget
    /// stops carry `OperationTerminal::Stopped`).
    terminal: Option<orca_core::budget::OperationTerminal>,
    verification: Option<orca_core::verification::VerificationResult>,
}

enum PreparedThreadTurnOutcome {
    Completed(ThreadTurnCompletion),
    ProviderSuspended {
        suspension: Box<RuntimeProviderSuspension>,
        background_workflows: RuntimeBackgroundWorkflows,
    },
}

pub enum ThreadTurnOutcome {
    Completed {
        status: RunStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        background_workflows: RuntimeBackgroundWorkflows,
        /// Typed operation terminal; the authoritative fact when present.
        terminal: Option<orca_core::budget::OperationTerminal>,
        verification: Option<orca_core::verification::VerificationResult>,
    },
    ProviderSuspended {
        suspension: Box<RuntimeProviderSuspension>,
        background_workflows: RuntimeBackgroundWorkflows,
    },
}

#[derive(Default)]
pub struct RuntimeBackgroundWorkflows(Vec<BackgroundWorkflowRun>);

impl RuntimeBackgroundWorkflows {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn from_vec(workflows: Vec<BackgroundWorkflowRun>) -> Self {
        Self(workflows)
    }

    pub(crate) fn into_inner(self) -> Vec<BackgroundWorkflowRun> {
        self.0
    }

    fn join_silently(self) {
        for workflow in self.0 {
            workflow.join_silently();
        }
    }
}

#[derive(Clone)]
pub struct ThreadTurnRequest {
    turn_id: TurnId,
    prompt: String,
    images: Vec<orca_core::conversation::ImageInput>,
    prompt_placement: ThreadTurnPromptPlacement,
    tool_mode: ThreadTurnToolMode,
    goal_turn_origin: Option<orca_core::goal_runtime::GoalTurnOrigin>,
    main_session_task_id: Option<String>,
    root_task_id: Option<String>,
    options: ControllerRunOptions,
    emit_session_completed: bool,
    steer_handle: Option<ThreadSteerHandle>,
    approval_handler: Option<Arc<dyn RuntimeApprovalHandler + Send + Sync>>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    user_input_handler: Option<Arc<dyn RuntimeUserInputHandler>>,
    mcp_elicitation_handler: Option<Arc<dyn McpElicitationHandler + Send + Sync>>,
    event_observer: Option<Arc<dyn EventObserver>>,
    continuation: Option<RuntimeTurnContinuation>,
    provider_suspension_control: Option<Arc<dyn RuntimeProviderSuspensionControl>>,
    provider_response_ingress: Option<Arc<dyn RuntimeProviderResponseIngress>>,
    workflow_lifecycle_ingress: Option<Arc<dyn RuntimeWorkflowLifecycleIngress>>,
    defer_cancel_terminal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadTurnPromptPlacement {
    BacktrackableUser,
    PinnedUser,
    PinnedSystem,
    ExistingTurn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadTurnToolMode {
    Standard,
    Goal,
}

impl<'a> ThreadTurnExecutor<'a> {
    pub fn new(
        config: &'a RunConfig,
        session: &'a mut InteractiveSession,
        lifecycle: &'a mut RuntimeSessionLifecycle,
    ) -> Self {
        Self {
            config,
            session,
            lifecycle,
            thread_extensions: None,
            turn_extension_id: None,
        }
    }

    pub(crate) fn new_with_thread_extensions(
        config: &'a RunConfig,
        session: &'a mut InteractiveSession,
        lifecycle: &'a mut RuntimeSessionLifecycle,
        thread_extensions: Arc<ExtensionData>,
        turn_extension_id: impl Into<String>,
    ) -> Self {
        Self {
            config,
            session,
            lifecycle,
            thread_extensions: Some(thread_extensions),
            turn_extension_id: Some(turn_extension_id.into()),
        }
    }

    pub fn run<W: io::Write>(&mut self, prompt: &str, writer: W) -> io::Result<RunStatus> {
        self.run_request(&ThreadTurnRequest::new(prompt), writer)
    }

    pub fn run_request<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
    ) -> io::Result<RunStatus> {
        self.run_request_with_cancel(request, writer, CancelToken::new())
    }

    pub fn run_request_with_cancel<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        run_thread_turn_inner(
            self.config,
            self.session,
            self.lifecycle,
            request,
            writer,
            cancel,
            self.thread_extensions.clone(),
            self.turn_extension_id.clone(),
        )
    }

    pub fn run_request_with_event_factory<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
    ) -> io::Result<RunStatus> {
        self.run_request_with_event_factory_and_cancel(request, writer, events, CancelToken::new())
    }

    pub fn run_request_with_event_factory_and_cancel<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        run_thread_turn_inner_with_events(
            self.config,
            self.session,
            self.lifecycle,
            request,
            writer,
            cancel,
            Some(events),
            self.thread_extensions.clone(),
            self.turn_extension_id.clone(),
        )
    }

    pub fn run_request_with_event_factory_and_cancel_outcome<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<ThreadTurnOutcome> {
        run_thread_turn_inner_with_events_outcome(
            self.config,
            self.session,
            self.lifecycle,
            request,
            writer,
            cancel,
            Some(events),
            self.thread_extensions.clone(),
            self.turn_extension_id.clone(),
        )
    }
}

impl<'a> ThreadTurnContext<'a> {
    pub fn prepare(
        config: &RunConfig,
        session: &'a mut InteractiveSession,
        request: &ThreadTurnRequest,
    ) -> io::Result<Self> {
        let cwd = config.cwd.clone().unwrap_or(std::env::current_dir()?);
        let prompt = request.prompt().to_string();
        let images = request.images().to_vec();
        if request.prompt_placement() != ThreadTurnPromptPlacement::ExistingTurn {
            session.wait_for_automatic_memory_snapshot();
        }
        let memory_start = session.begin_automatic_memory_turn(
            request.turn_id().as_str(),
            request.prompt_placement() == ThreadTurnPromptPlacement::ExistingTurn,
        );
        let mut parts = session.runtime_parts();
        if let Some(writer) = parts.writer.as_deref_mut() {
            writer.enter_turn(request.turn_id().clone());
        }
        parts
            .conversation
            .replace_mode_context(agent_common::mode_context(config.approval_mode));
        crate::memory::refresh_project_memory_context(
            parts.conversation,
            &cwd,
            &prompt,
            config.auto_memory && !matches!(config.history_mode, HistoryMode::Disabled),
            request.prompt_placement() == ThreadTurnPromptPlacement::ExistingTurn,
        );
        if request.prompt_placement() != ThreadTurnPromptPlacement::ExistingTurn {
            parts
                .conversation
                .replace_skill_context(agent_common::explicit_skill_context(&cwd, &prompt));
            let message = match request.prompt_placement() {
                ThreadTurnPromptPlacement::BacktrackableUser => {
                    Message::user_with_images(prompt.clone(), images)
                }
                ThreadTurnPromptPlacement::PinnedUser => {
                    Message::pinned_user_with_images(prompt.clone(), images)
                }
                ThreadTurnPromptPlacement::PinnedSystem => Message::pinned_system(prompt.clone()),
                ThreadTurnPromptPlacement::ExistingTurn => unreachable!(),
            };
            if let Some(writer) = parts.writer.as_deref_mut() {
                writer.append_message(&message)?;
            }
            parts.conversation.messages.push(message);
        }

        Ok(Self {
            cwd,
            prompt,
            memory_start,
            parts,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

impl<W: io::Write> ThreadTurnExecution<W> {
    pub fn new(
        lifecycle: &RuntimeSessionLifecycle,
        writer: W,
        output_format: OutputFormat,
    ) -> Self {
        Self::new_with_cancel(lifecycle, writer, output_format, CancelToken::new())
    }

    pub fn new_with_cancel(
        lifecycle: &RuntimeSessionLifecycle,
        writer: W,
        output_format: OutputFormat,
        cancel: CancelToken,
    ) -> Self {
        Self::new_with_cancel_and_observer(lifecycle, writer, output_format, cancel, None)
    }

    fn new_with_cancel_and_observer(
        lifecycle: &RuntimeSessionLifecycle,
        writer: W,
        output_format: OutputFormat,
        cancel: CancelToken,
        event_observer: Option<Arc<dyn EventObserver>>,
    ) -> Self {
        Self::new_with_events(
            EventFactory::new(lifecycle.run_id().to_string()),
            writer,
            output_format,
            cancel,
            event_observer,
        )
    }

    fn new_with_events(
        events: EventFactory,
        writer: W,
        output_format: OutputFormat,
        cancel: CancelToken,
        event_observer: Option<Arc<dyn EventObserver>>,
    ) -> Self {
        Self {
            events,
            sink: EventSink::new(writer, output_format).with_optional_observer(event_observer),
            cancel,
            background_workflows: Vec::new(),
        }
    }

    pub fn run_id(&self) -> &str {
        self.events.run_id()
    }

    pub fn background_workflow_count(&self) -> usize {
        self.background_workflows.len()
    }
}

impl ThreadTurnMainSessionTask {
    fn from_request(
        request: &ThreadTurnRequest,
        registry: &TaskRegistry,
        usage_before: UsageTotals,
    ) -> Option<Self> {
        request.main_session_task_id().map(|id| Self {
            registry: registry.clone(),
            id: id.to_string(),
            usage_before,
            task_usage_before: registry
                .get(id)
                .and_then(|task| task.usage)
                .unwrap_or_default(),
        })
    }

    fn emit_current<W: io::Write>(
        &self,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
    ) -> io::Result<()> {
        let task = self
            .registry
            .list()
            .into_iter()
            .find(|task| task.id == self.id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("main-session task '{}' not found", self.id),
                )
            })?;
        sink.emit(events.task_status_updated(&task))
    }

    fn emit_all<W: io::Write>(
        &self,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
    ) -> io::Result<()> {
        sink.emit(events.workflow_tasks_updated(&self.registry.list()))
    }

    fn finish(&self, status: RunStatus, error: Option<&str>, usage: UsageTotals) -> io::Result<()> {
        let usage = add_task_usage(
            self.task_usage_before,
            task_usage_delta(self.usage_before, usage),
        );
        let result = match status {
            RunStatus::Success => self
                .registry
                .apply_main_session_terminal_update(
                    &self.id,
                    MainSessionTerminalUpdate::Completed {
                        result: status.as_str().to_string(),
                    },
                    Some(usage),
                )
                .map(|_| ()),
            RunStatus::Cancelled => {
                self.registry
                    .stop_with_usage(&self.id, status.as_str().to_string(), Some(usage))
            }
            RunStatus::Failed | RunStatus::ApprovalRequired | RunStatus::VerificationFailed => self
                .registry
                .apply_main_session_terminal_update(
                    &self.id,
                    MainSessionTerminalUpdate::Failed {
                        error: error.unwrap_or(status.as_str()).to_string(),
                    },
                    Some(usage),
                )
                .map(|_| ()),
        };
        result.map_err(io::Error::other)
    }

    fn finish_and_emit<W: io::Write>(
        &self,
        status: RunStatus,
        error: Option<&str>,
        usage: UsageTotals,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
    ) -> io::Result<()> {
        self.finish(status, error, usage)?;
        self.emit_current(events, sink)
    }
}

fn task_usage_delta(before: UsageTotals, after: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        cache_tokens: after.cache_tokens.saturating_sub(before.cache_tokens),
        estimated_cost_usd: (after.estimated_cost_usd - before.estimated_cost_usd).max(0.0),
    }
}

fn add_task_usage(base: UsageTotals, delta: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: base.input_tokens.saturating_add(delta.input_tokens),
        output_tokens: base.output_tokens.saturating_add(delta.output_tokens),
        cache_tokens: base.cache_tokens.saturating_add(delta.cache_tokens),
        estimated_cost_usd: base.estimated_cost_usd + delta.estimated_cost_usd,
    }
}

impl<'a, 'session, W: io::Write> PreparedThreadTurn<'a, 'session, W> {
    fn execute(self) -> io::Result<PreparedThreadTurnOutcome> {
        let Self {
            config,
            lifecycle,
            request,
            context,
            cancel,
            events,
            sink,
            background_workflows,
            thread_extensions,
            turn_extension_id,
        } = self;
        let ThreadTurnContext {
            cwd,
            prompt,
            memory_start,
            parts,
        } = context;
        let main_session_task = ThreadTurnMainSessionTask::from_request(
            request,
            parts.task_registry,
            parts.cost_tracker.totals(),
        );
        if let Some(task) = main_session_task.as_ref()
            && let Err(error) = task.emit_current(events, sink)
        {
            task.finish(
                RunStatus::Failed,
                Some(&error.to_string()),
                parts.cost_tracker.totals(),
            )?;
            return Err(error);
        }

        let loop_context = AgentLoopContext::new(&cwd, &prompt, 0, true, &SubagentType::General)
            .with_turn_id(request.turn_id().clone())
            .with_deferred_cancel_terminal(request.defer_cancel_terminal())
            .with_root_task_id(request.root_task_id())
            .with_services(
                parts.instructions,
                parts.memory,
                parts.mcp_registry,
                parts.hooks,
            );
        let loop_context = if let (Some(thread_extensions), Some(turn_extension_id)) =
            (thread_extensions, turn_extension_id)
        {
            loop_context.with_runtime_thread_extensions(
                parts.cost_tracker,
                cancel,
                parts.task_registry,
                thread_extensions,
                turn_extension_id,
            )
        } else {
            loop_context.with_runtime(parts.cost_tracker, cancel, parts.task_registry)
        };
        let loop_context = if let Some(continuation) = request.continuation().cloned() {
            loop_context.with_turn_continuation(continuation)
        } else {
            loop_context
        }
        .with_provider_suspension_control(request.provider_suspension_control())
        .with_provider_response_ingress(request.provider_response_ingress())
        .with_workflow_lifecycle_ingress(request.workflow_lifecycle_ingress())
        .with_wait_for_background_workflows(request.options().wait_for_background_workflows);
        let turn_result = (|| -> io::Result<AgentLoopOutcome> {
            run_agent_loop(
                config,
                loop_context
                    .with_execution(background_workflows, None, Some(lifecycle))
                    .with_steer_handle(request.steer_handle())
                    .with_approval_handler(request.approval_handler())
                    .with_permission_handler(request.permission_handler())
                    .with_user_input_handler(request.user_input_handler())
                    .with_mcp_elicitation_handler(request.mcp_elicitation_handler()),
                events,
                sink,
                AgentConversationContext::borrowed(parts.conversation, parts.writer),
                request.tool_mode().policy(),
            )
        })();
        let usage = parts.cost_tracker.totals();
        let result = match turn_result {
            Ok(AgentLoopOutcome::ProviderSuspended(suspension)) => {
                return Ok(PreparedThreadTurnOutcome::ProviderSuspended {
                    suspension: Box::new(suspension),
                    background_workflows: RuntimeBackgroundWorkflows::from_vec(std::mem::take(
                        background_workflows,
                    )),
                });
            }
            Ok(AgentLoopOutcome::Completed(result)) => result,
            Err(error) => {
                if let Some(task) = main_session_task.as_ref() {
                    task.finish_and_emit(
                        RunStatus::Failed,
                        Some(&error.to_string()),
                        usage,
                        events,
                        sink,
                    )?;
                }
                return Err(error);
            }
        };
        let completion =
            (|| -> io::Result<(
                RunStatus,
                crate::lifecycle::TurnEndReason,
                Option<String>,
                Option<orca_core::verification::VerificationResult>,
            )> {
                let status = result.status;
                let mut end_reason = result.reason;
                lifecycle.finish_task(status);
                if request.options().wait_for_background_workflows {
                    observe_background_workflows(
                        true,
                        events,
                        sink,
                        background_workflows,
                        parts.task_registry,
                        cancel,
                        request.workflow_lifecycle_ingress(),
                    )?;
                }
                let (status, verification) =
                    run_verifier_if_needed(status, config.verifier.as_deref(), events, sink)?;
                if status != result.status {
                    // Verifier (or another post-loop step) changed the terminal
                    // status; the original end reason no longer describes it.
                    end_reason = crate::lifecycle::TurnEndReason::Unclassified;
                }
                Ok((status, end_reason, result.error, verification))
            })();
        let (status, end_reason, error, verification) = match completion {
            Ok(completion) => completion,
            Err(error) => {
                if let Some(task) = main_session_task.as_ref() {
                    task.finish_and_emit(
                        RunStatus::Failed,
                        Some(&error.to_string()),
                        usage,
                        events,
                        sink,
                    )?;
                }
                return Err(error);
            }
        };

        let background_workflows =
            RuntimeBackgroundWorkflows::from_vec(std::mem::take(background_workflows));
        Ok(PreparedThreadTurnOutcome::Completed(ThreadTurnCompletion {
            cwd,
            memory_start,
            status,
            end_reason,
            error,
            usage,
            main_session_task,
            background_workflows,
            terminal: Some(result.terminal),
            verification,
        }))
    }
}

impl ThreadTurnCompletion {
    fn commit<W: io::Write>(
        self,
        config: &RunConfig,
        session: &mut InteractiveSession,
        request: &ThreadTurnRequest,
        cancel: &CancelToken,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
    ) -> io::Result<RunStatus> {
        // Legacy statuses that predate typed terminals keep their transcript
        // status: ApprovalRequired and VerificationFailed are distinct
        // outcomes with their own exit codes, not generic runtime failures.
        // Budget stops (and every other terminal) use the typed terminal.
        if let Some(terminal) = self.terminal.as_ref()
            && matches!(
                self.status,
                RunStatus::ApprovalRequired | RunStatus::VerificationFailed
            )
            && !matches!(
                terminal,
                orca_core::budget::OperationTerminal::Stopped { .. }
            )
        {
            let _ =
                session.complete_with_error_durable(self.status.as_str(), self.error.as_deref());
            if let Some(task) = self.main_session_task.as_ref() {
                task.finish_and_emit(self.status, self.error.as_deref(), self.usage, events, sink)?;
                task.emit_all(events, sink)?;
            }
            if request.emit_session_completed() {
                sink.emit(events.session_completed_with_verification(
                    self.status,
                    session.session_id(),
                    self.verification.as_ref(),
                ))?;
            }
            return Ok(self.status);
        }
        let transcript_committed = if let Some(terminal) = self.terminal.as_ref() {
            session.complete_with_terminal_durable(terminal)
        } else {
            session.complete_with_error_durable(self.status.as_str(), self.error.as_deref())
        };
        if let Some(task) = self.main_session_task.as_ref() {
            task.finish_and_emit(self.status, self.error.as_deref(), self.usage, events, sink)?;
            task.emit_all(events, sink)?;
        }
        if request.emit_session_completed() {
            if let Some(terminal) = self.terminal.as_ref() {
                sink.emit(events.session_completed_terminal_with_verification(
                    terminal,
                    session.session_id(),
                    self.verification.as_ref(),
                ))?;
            } else {
                sink.emit(events.session_completed(self.status, session.session_id()))?;
            }
        }
        if crate::memory::automatic_memory_capture_is_eligible(
            config,
            self.status,
            transcript_committed,
            request.emit_session_completed(),
        ) {
            if let Err(error) = session.enqueue_automatic_memory_turn(
                config,
                &self.cwd,
                self.memory_start,
                request.turn_id().as_str(),
                cancel,
            ) {
                eprintln!("orca: warning: automatic memory extraction failed: {error}");
            }
        } else {
            session.finish_automatic_memory_turn(request.turn_id().as_str());
        }
        Ok(self.status)
    }
}

impl PreparedThreadTurnOutcome {
    fn commit<W: io::Write>(
        self,
        config: &RunConfig,
        session: &mut InteractiveSession,
        request: &ThreadTurnRequest,
        cancel: &CancelToken,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
    ) -> io::Result<ThreadTurnOutcome> {
        match self {
            Self::Completed(mut completion) => {
                let background_workflows = std::mem::take(&mut completion.background_workflows);
                let end_reason = completion.end_reason;
                let terminal = completion.terminal.clone();
                let verification = completion.verification.clone();
                completion
                    .commit(config, session, request, cancel, events, sink)
                    .map(|status| ThreadTurnOutcome::Completed {
                        status,
                        end_reason,
                        background_workflows,
                        terminal,
                        verification,
                    })
            }
            Self::ProviderSuspended {
                suspension,
                background_workflows,
            } => Ok(ThreadTurnOutcome::ProviderSuspended {
                suspension,
                background_workflows,
            }),
        }
    }
}

impl ThreadTurnOutcome {
    fn into_completed(self) -> io::Result<RunStatus> {
        match self {
            Self::Completed {
                status,
                end_reason: _,
                background_workflows,
                terminal,
                ..
            } => {
                background_workflows.join_silently();
                // The typed terminal's exit code is authoritative for budget
                // stops (4); plain statuses keep the legacy mapping.
                Ok(terminal
                    .map(orca_core::budget::OperationTerminal::exit_code)
                    .map_or(status, |_| status))
            }
            Self::ProviderSuspended {
                suspension,
                background_workflows,
            } => {
                background_workflows.join_silently();
                drop(suspension);
                Err(io::Error::other(
                    "provider turn suspended without a suspension-aware caller",
                ))
            }
        }
    }
}

impl ThreadTurnRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            turn_id: TurnId::new(),
            prompt: prompt.into(),
            images: Vec::new(),
            prompt_placement: ThreadTurnPromptPlacement::BacktrackableUser,
            tool_mode: ThreadTurnToolMode::Standard,
            goal_turn_origin: None,
            main_session_task_id: None,
            root_task_id: None,
            options: ControllerRunOptions::default(),
            emit_session_completed: true,
            steer_handle: None,
            approval_handler: None,
            permission_handler: None,
            user_input_handler: None,
            mcp_elicitation_handler: None,
            event_observer: None,
            continuation: None,
            provider_suspension_control: None,
            provider_response_ingress: None,
            workflow_lifecycle_ingress: None,
            defer_cancel_terminal: false,
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn with_images(mut self, images: Vec<orca_core::conversation::ImageInput>) -> Self {
        self.images = images;
        self
    }

    pub fn images(&self) -> &[orca_core::conversation::ImageInput] {
        &self.images
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub fn options(&self) -> ControllerRunOptions {
        self.options
    }

    pub fn with_options(mut self, options: ControllerRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_prompt_placement(mut self, placement: ThreadTurnPromptPlacement) -> Self {
        self.prompt_placement = placement;
        self
    }

    pub fn prompt_placement(&self) -> ThreadTurnPromptPlacement {
        self.prompt_placement
    }

    pub fn with_tool_mode(mut self, tool_mode: ThreadTurnToolMode) -> Self {
        self.tool_mode = tool_mode;
        self
    }

    pub fn tool_mode(&self) -> ThreadTurnToolMode {
        self.tool_mode
    }

    pub fn with_goal_turn_origin(
        mut self,
        origin: orca_core::goal_runtime::GoalTurnOrigin,
    ) -> Self {
        self.goal_turn_origin = Some(origin);
        self
    }

    pub fn goal_turn_origin(&self) -> Option<orca_core::goal_runtime::GoalTurnOrigin> {
        self.goal_turn_origin
    }

    pub fn with_main_session_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.main_session_task_id = Some(task_id.into());
        self
    }

    pub fn main_session_task_id(&self) -> Option<&str> {
        self.main_session_task_id.as_deref()
    }

    pub(crate) fn with_root_task_id(mut self, root_task_id: impl Into<String>) -> Self {
        self.root_task_id = Some(root_task_id.into());
        self
    }

    pub(crate) fn root_task_id(&self) -> Option<&str> {
        self.root_task_id.as_deref()
    }

    pub fn with_wait_for_background_workflows(mut self, wait: bool) -> Self {
        self.options.wait_for_background_workflows = wait;
        self
    }

    pub fn with_session_completed_event(mut self, emit: bool) -> Self {
        self.emit_session_completed = emit;
        self
    }

    pub fn emit_session_completed(&self) -> bool {
        self.emit_session_completed
    }

    pub fn with_steer_handle(mut self, handle: ThreadSteerHandle) -> Self {
        self.steer_handle = Some(handle);
        self
    }

    pub fn with_permission_handler(
        mut self,
        handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    ) -> Self {
        self.permission_handler = Some(handler);
        self
    }

    pub fn with_approval_handler(
        mut self,
        handler: Arc<dyn RuntimeApprovalHandler + Send + Sync>,
    ) -> Self {
        self.approval_handler = Some(handler);
        self
    }

    pub fn with_user_input_handler(mut self, handler: Arc<dyn RuntimeUserInputHandler>) -> Self {
        self.user_input_handler = Some(handler);
        self
    }

    pub fn with_threaded_user_input_handler(
        mut self,
        handler: Arc<dyn RuntimeUserInputHandler + Send + Sync>,
    ) -> Self {
        self.user_input_handler = Some(handler);
        self
    }

    pub fn with_mcp_elicitation_handler(
        mut self,
        handler: Arc<dyn McpElicitationHandler + Send + Sync>,
    ) -> Self {
        self.mcp_elicitation_handler = Some(handler);
        self
    }

    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = Some(observer);
        self
    }

    pub fn with_provider_suspension_control(
        mut self,
        control: Arc<dyn RuntimeProviderSuspensionControl>,
    ) -> Self {
        self.provider_suspension_control = Some(control);
        self
    }

    pub fn with_provider_response_ingress(
        mut self,
        ingress: Arc<dyn RuntimeProviderResponseIngress>,
    ) -> Self {
        self.provider_response_ingress = Some(ingress);
        self
    }

    pub fn with_workflow_lifecycle_ingress(
        mut self,
        ingress: Arc<dyn RuntimeWorkflowLifecycleIngress>,
    ) -> Self {
        self.workflow_lifecycle_ingress = Some(ingress);
        self
    }

    pub fn with_continuation(mut self, continuation: RuntimeTurnContinuation) -> Self {
        self.continuation = Some(continuation);
        self.prompt_placement = ThreadTurnPromptPlacement::ExistingTurn;
        self
    }

    pub fn with_existing_turn_prompt(mut self) -> Self {
        self.prompt_placement = ThreadTurnPromptPlacement::ExistingTurn;
        self
    }

    pub(crate) fn with_deferred_cancel_terminal(mut self, defer: bool) -> Self {
        self.defer_cancel_terminal = defer;
        self
    }

    fn defer_cancel_terminal(&self) -> bool {
        self.defer_cancel_terminal
    }

    pub fn steer_handle(&self) -> Option<&ThreadSteerHandle> {
        self.steer_handle.as_ref()
    }

    pub fn permission_handler(
        &self,
    ) -> Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)> {
        self.permission_handler.as_deref()
    }

    pub fn approval_handler(&self) -> Option<&(dyn RuntimeApprovalHandler + Send + Sync)> {
        self.approval_handler.as_deref()
    }

    pub fn user_input_handler(&self) -> Option<&dyn RuntimeUserInputHandler> {
        self.user_input_handler.as_deref()
    }

    pub fn mcp_elicitation_handler(&self) -> Option<&(dyn McpElicitationHandler + Send + Sync)> {
        self.mcp_elicitation_handler.as_deref()
    }

    pub fn event_observer(&self) -> Option<&Arc<dyn EventObserver>> {
        self.event_observer.as_ref()
    }

    pub fn provider_response_ingress(&self) -> Option<&dyn RuntimeProviderResponseIngress> {
        self.provider_response_ingress.as_deref()
    }

    pub fn workflow_lifecycle_ingress(&self) -> Option<&dyn RuntimeWorkflowLifecycleIngress> {
        self.workflow_lifecycle_ingress.as_deref()
    }

    pub fn continuation(&self) -> Option<&RuntimeTurnContinuation> {
        self.continuation.as_ref()
    }

    pub fn provider_suspension_control(&self) -> Option<&dyn RuntimeProviderSuspensionControl> {
        self.provider_suspension_control.as_deref()
    }
}

impl ThreadTurnToolMode {
    fn policy(self) -> AgentToolPolicyContext<'static> {
        match self {
            Self::Standard => AgentToolPolicyContext::unrestricted(),
            Self::Goal => AgentToolPolicyContext::goal_mode(),
        }
    }
}

pub fn run(config: RunConfig) -> i32 {
    let stdout = io::stdout();
    let options = ControllerRunOptions::for_run_config(&config);
    match run_inner(config, stdout.lock(), options, None) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

pub fn run_to_writer<W: io::Write>(config: RunConfig, writer: W) -> i32 {
    let options = ControllerRunOptions::for_run_config(&config);
    run_to_writer_with_options(config, writer, options)
}

pub fn run_to_writer_with_options<W: io::Write>(
    config: RunConfig,
    writer: W,
    options: ControllerRunOptions,
) -> i32 {
    match run_inner(config, writer, options, None) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

/// Run one Headless operation with a typed runtime-surface interaction
/// transport. Existing run/run_to_writer callers remain non-interactive.
pub fn run_to_writer_with_headless_transport<W: io::Write>(
    config: RunConfig,
    writer: W,
    options: ControllerRunOptions,
    transport: HeadlessInteractionTransport,
) -> i32 {
    match run_inner(config, writer, options, Some(transport)) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

fn run_inner<W: io::Write>(
    config: RunConfig,
    mut writer: W,
    _options: ControllerRunOptions,
    transport: Option<HeadlessInteractionTransport>,
) -> io::Result<i32> {
    let prompt = if config.prompt.trim().is_empty() {
        "(empty prompt)".to_string()
    } else {
        config.prompt.trim().to_string()
    };

    let host = RuntimeHost::start().map_err(runtime_host_io_error)?;
    let mut start_request = RuntimeThreadStartRequest::new(config.clone(), prompt.as_str());
    if matches!(config.history_mode, HistoryMode::Disabled) {
        start_request = start_request.with_ephemeral_non_catalogued_one_shot(
            crate::runtime_surface::FirstOperationCompletionPolicy::Terminal,
        );
    }
    let thread = host
        .start_thread_with_request(start_request)
        .map_err(runtime_host_io_error)?;
    for error in thread.startup_warnings() {
        eprintln!("orca: warning: {error}");
    }
    let mut headless = thread.attach_headless_surface(transport.is_some())?;
    let (relay_tx, relay_rx) = mpsc::sync_channel(HOSTED_EVENT_RELAY_CAPACITY);
    let operation = headless.start_turn(
        &prompt,
        HostedEventRelayWriter {
            tx: relay_tx,
            buffer: Vec::new(),
        },
    )?;
    let terminal = drain_headless_events(
        &operation,
        &mut headless,
        transport.as_ref(),
        config.output_format == OutputFormat::Jsonl,
        relay_rx,
        &mut writer,
    );
    let shutdown = match host.shutdown() {
        Ok(()) | Err(RuntimeHostError::ThreadUnavailable) => Ok(()),
        Err(error) => Err(runtime_host_io_error(error)),
    };
    let terminal = terminal?;
    let status = headless_operation_status(&terminal);
    let exit_code = headless_operation_exit_code(&terminal);
    shutdown?;
    if config.desktop_notifications {
        let _ = crate::notify::notify("Orca", &format!("Session {}", status.as_str()));
    }
    if config.output_format == OutputFormat::Text
        && status != RunStatus::Success
        && let Some(session_id) = thread.session_id()
    {
        writeln!(
            writer,
            "To continue this session, run: orca exec resume {session_id}"
        )?;
    }
    Ok(exit_code)
}

fn drain_headless_events<W: io::Write>(
    operation: &HeadlessOperationHandle,
    session: &mut HeadlessSurfaceSession,
    transport: Option<&HeadlessInteractionTransport>,
    preserve_jsonl_session_boundary: bool,
    relay_rx: mpsc::Receiver<HostedEventChunk>,
    writer: &mut W,
) -> io::Result<crate::runtime_surface::OperationTerminalAtCursor> {
    let mut pending = None;
    let mut session_started_written = !preserve_jsonl_session_boundary;
    let mut before_session_started = Vec::new();
    loop {
        if let Some(transport) = transport {
            if pending.is_none()
                && let Some(checkpoint) = session.try_next_interaction()?
            {
                if transport.callback_unavailable.load(Ordering::Acquire)
                    || transport.callback_in_flight.swap(true, Ordering::AcqRel)
                {
                    let answer = fail_closed_headless_answer(&checkpoint)?;
                    session.respond_interaction(&checkpoint, answer)?;
                } else {
                    let handler = Arc::clone(&transport.handler);
                    let callback_in_flight = Arc::clone(&transport.callback_in_flight);
                    let callback_checkpoint = checkpoint.clone();
                    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
                    std::thread::spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            handler.handle(&callback_checkpoint)
                        }))
                        .unwrap_or_else(|_| {
                            Err(io::Error::other("Headless interaction handler panicked"))
                        });
                        callback_in_flight.store(false, Ordering::Release);
                        let _ = answer_tx.send(result);
                    });
                    pending = Some(PendingHeadlessInteraction {
                        checkpoint,
                        answer_rx,
                        deadline: std::time::Instant::now() + transport.timeout,
                    });
                }
            }
            if let Some(active) = pending.as_ref() {
                let answer = match active.answer_rx.try_recv() {
                    Ok(Ok(answer)) if headless_answer_matches(&active.checkpoint, &answer) => {
                        Some(answer)
                    }
                    Ok(Ok(_)) | Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                        Some(fail_closed_headless_answer(&active.checkpoint)?)
                    }
                    Err(mpsc::TryRecvError::Empty)
                        if std::time::Instant::now() >= active.deadline =>
                    {
                        transport
                            .callback_unavailable
                            .store(true, Ordering::Release);
                        Some(fail_closed_headless_answer(&active.checkpoint)?)
                    }
                    Err(mpsc::TryRecvError::Empty) => None,
                };
                if let Some(answer) = answer {
                    let active = pending.take().expect("pending Headless interaction exists");
                    session.respond_interaction(&active.checkpoint, answer)?;
                }
            }
        }
        match relay_rx.recv_timeout(HOSTED_EVENT_RELAY_POLL) {
            Ok(chunk) => {
                if !session_started_written
                    && !headless_chunk_contains_session_started(&chunk.bytes)
                {
                    before_session_started.push(chunk.bytes);
                    let _ = chunk.ack.send(Ok(()));
                    continue;
                }
                let result = writer.write_all(&chunk.bytes).and_then(|()| writer.flush());
                if result.is_ok() && !session_started_written {
                    session_started_written = true;
                    for bytes in before_session_started.drain(..) {
                        writer.write_all(&bytes)?;
                    }
                    writer.flush()?;
                }
                let acknowledgement = result
                    .as_ref()
                    .map(|()| ())
                    .map_err(HostedEventRelayError::from_io);
                let _ = chunk.ack.send(acknowledgement);
                result?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
        if let Some(terminal) = operation.try_terminal()? {
            return Ok(terminal);
        }
    }
}

fn headless_chunk_contains_session_started(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).ok().is_some_and(|text| {
        text.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|event| event["type"] == "session.started")
        })
    })
}

fn headless_answer_matches(
    checkpoint: &HeadlessInteractionCheckpoint,
    answer: &SurfaceClientInteractionAnswer,
) -> bool {
    matches!(
        (&checkpoint.interaction.request, answer),
        (
            SurfaceInteractionRequest::ToolApproval { .. },
            SurfaceClientInteractionAnswer::ToolApproval { .. }
        ) | (
            SurfaceInteractionRequest::PermissionRequest { .. },
            SurfaceClientInteractionAnswer::PermissionRequest { .. }
        ) | (
            SurfaceInteractionRequest::UserInput { .. },
            SurfaceClientInteractionAnswer::UserInput { .. }
        ) | (
            SurfaceInteractionRequest::McpElicitation { .. },
            SurfaceClientInteractionAnswer::McpElicitation { .. }
        )
    )
}

fn fail_closed_headless_answer(
    checkpoint: &HeadlessInteractionCheckpoint,
) -> io::Result<SurfaceClientInteractionAnswer> {
    match &checkpoint.interaction.request {
        SurfaceInteractionRequest::ToolApproval { .. } => {
            Ok(SurfaceClientInteractionAnswer::ToolApproval {
                decision: SurfaceAllowDeny::Deny,
            })
        }
        SurfaceInteractionRequest::PermissionRequest { permissions, .. } => {
            Ok(SurfaceClientInteractionAnswer::PermissionRequest {
                decision: SurfacePermissionClientDecision::Deny {
                    scope: crate::runtime_surface::PermissionGrantScope::Turn,
                    permissions: permissions.clone(),
                    strict_auto_review: false,
                },
            })
        }
        SurfaceInteractionRequest::UserInput { .. } => {
            Ok(SurfaceClientInteractionAnswer::UserInput {
                decision: SurfaceUserInputDecision::Cancel,
            })
        }
        SurfaceInteractionRequest::McpElicitation { .. } => {
            Ok(SurfaceClientInteractionAnswer::McpElicitation {
                decision: SurfaceMcpElicitationDecision::Decline,
            })
        }
        SurfaceInteractionRequest::BackgroundApproval { .. } => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Headless transport does not accept background approvals",
        )),
    }
}

fn headless_operation_status(
    terminal: &crate::runtime_surface::OperationTerminalAtCursor,
) -> RunStatus {
    match &terminal.terminal {
        SurfaceOperationTerminal::Succeeded { .. } => RunStatus::Success,
        SurfaceOperationTerminal::Cancelled { .. } => RunStatus::Cancelled,
        SurfaceOperationTerminal::Failed {
            class: SurfaceFailureClass::LegacyApprovalRequired,
            ..
        } => RunStatus::ApprovalRequired,
        SurfaceOperationTerminal::Failed {
            class: SurfaceFailureClass::Verification,
            ..
        } => RunStatus::VerificationFailed,
        _ => RunStatus::Failed,
    }
}

fn headless_operation_exit_code(
    terminal: &crate::runtime_surface::OperationTerminalAtCursor,
) -> i32 {
    match terminal.terminal {
        SurfaceOperationTerminal::BudgetExhausted { .. } => 4,
        _ => headless_operation_status(terminal).exit_code(),
    }
}

fn runtime_host_io_error(error: RuntimeHostError) -> io::Error {
    io::Error::other(error)
}

pub fn run_thread_turn_to_writer<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    prompt: &str,
    writer: W,
    options: ControllerRunOptions,
) -> io::Result<RunStatus> {
    ThreadTurnExecutor::new(config, session, lifecycle).run_request(
        &ThreadTurnRequest::new(prompt).with_options(options),
        writer,
    )
}

pub fn run_thread_turn_to_writer_with_cancel<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    prompt: &str,
    writer: W,
    options: ControllerRunOptions,
    cancel: CancelToken,
) -> io::Result<RunStatus> {
    run_thread_turn_inner(
        config,
        session,
        lifecycle,
        &ThreadTurnRequest::new(prompt).with_options(options),
        writer,
        cancel,
        None,
        None,
    )
}

fn run_thread_turn_inner<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    request: &ThreadTurnRequest,
    writer: W,
    cancel: CancelToken,
    thread_extensions: Option<Arc<ExtensionData>>,
    turn_extension_id: Option<String>,
) -> io::Result<RunStatus> {
    run_thread_turn_inner_with_events(
        config,
        session,
        lifecycle,
        request,
        writer,
        cancel,
        None,
        thread_extensions,
        turn_extension_id,
    )
}

fn run_thread_turn_inner_with_events<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    request: &ThreadTurnRequest,
    writer: W,
    cancel: CancelToken,
    events: Option<&mut EventFactory>,
    thread_extensions: Option<Arc<ExtensionData>>,
    turn_extension_id: Option<String>,
) -> io::Result<RunStatus> {
    run_thread_turn_inner_with_events_outcome(
        config,
        session,
        lifecycle,
        request,
        writer,
        cancel,
        events,
        thread_extensions,
        turn_extension_id,
    )?
    .into_completed()
}

fn run_thread_turn_inner_with_events_outcome<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    request: &ThreadTurnRequest,
    writer: W,
    cancel: CancelToken,
    events: Option<&mut EventFactory>,
    thread_extensions: Option<Arc<ExtensionData>>,
    turn_extension_id: Option<String>,
) -> io::Result<ThreadTurnOutcome> {
    drain_terminal_notifications(session, thread_extensions.as_deref());
    let context = ThreadTurnContext::prepare(config, session, request)?;
    if let Some(events) = events {
        let mut sink = EventSink::new(writer, config.output_format)
            .with_optional_observer(request.event_observer().cloned());
        let mut background_workflows = Vec::new();
        return PreparedThreadTurn {
            config,
            lifecycle,
            request,
            context,
            cancel: &cancel,
            events,
            sink: &mut sink,
            background_workflows: &mut background_workflows,
            thread_extensions,
            turn_extension_id,
        }
        .execute()?
        .commit(config, session, request, &cancel, events, &mut sink);
    }

    let mut execution = ThreadTurnExecution::new_with_cancel_and_observer(
        lifecycle,
        writer,
        config.output_format,
        cancel,
        request.event_observer().cloned(),
    );
    PreparedThreadTurn {
        config,
        lifecycle,
        request,
        context,
        cancel: &execution.cancel,
        events: &mut execution.events,
        sink: &mut execution.sink,
        background_workflows: &mut execution.background_workflows,
        thread_extensions,
        turn_extension_id,
    }
    .execute()?
    .commit(
        config,
        session,
        request,
        &execution.cancel,
        &mut execution.events,
        &mut execution.sink,
    )
}

fn drain_terminal_notifications(
    session: &mut InteractiveSession,
    thread_extensions: Option<&ExtensionData>,
) {
    let Some(service) = thread_extensions.and_then(ExtensionData::get::<TerminalService>) else {
        return;
    };
    for completion in service.drain_completions() {
        let message = Message::pinned_system(completion.model_notification());
        session.append_message(&message);
        session.conversation_mut().messages.push(message);
    }
}

#[cfg(test)]
fn canonical_action_for_tool(
    tool_request: &tool_types::ToolRequest,
    mcp_registry: &McpRegistry,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
) -> orca_core::approval_types::ActionKind {
    orca_tools::canonical_action_kind_with_mcp_and_external(
        tool_request,
        Some(mcp_registry),
        external_tools,
    )
}

fn run_verifier_if_needed(
    status: RunStatus,
    verifier: Option<&str>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
) -> io::Result<(
    RunStatus,
    Option<orca_core::verification::VerificationResult>,
)> {
    if status != RunStatus::Success {
        return Ok((status, None));
    }

    let Some(command) = verifier else {
        return Ok((status, None));
    };

    sink.emit(events.verification_started(command))?;
    let result = orca_core::verification::run(command);
    let success = result.success;
    sink.emit(events.verification_completed(&result))?;

    if success {
        Ok((RunStatus::Success, Some(result)))
    } else {
        Ok((RunStatus::VerificationFailed, Some(result)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::agent_loop::execute_child_agent_loop;
    use crate::hooks::HookOutcome;
    use crate::hooks::conversation_with_hook_context;
    use crate::lifecycle::{
        RuntimeTaskStatus, RuntimeToolActorContext, RuntimeUserInputHandler,
        RuntimeUserInputRequest,
    };
    use crate::memory::MemoryBlock;
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
        RequestPermissionProfile,
    };
    use crate::runtime_permission::{RuntimePermissionRequest, RuntimePermissionResponse};
    use crate::runtime_tool_call::{
        RuntimeNormalToolHandler, RuntimeNormalToolInvocation, RuntimeNormalToolWorkerContext,
        RuntimeToolCallRuntime,
    };
    use crate::subagent_execution::{collect_subagent_batch, should_run_subagent_batch};
    use crate::tool_execution::{
        ToolApprovalGateContext, ToolExecutionActor, ToolExecutionContext,
    };
    use crate::tool_invocation::prepare_tool_invocation;
    use crate::tool_router::{RuntimeToolInvocationContext, RuntimeToolRouter};
    use orca_approval::ApprovalPolicy;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind};
    use orca_core::conversation::Conversation;
    use orca_core::conversation::Message;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    struct PermissionCarryNormalHandler {
        calls: AtomicUsize,
    }

    impl RuntimeNormalToolHandler for PermissionCarryNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> tool_types::ToolResult {
            match self.calls.fetch_add(1, Ordering::AcqRel) {
                0 => {
                    context
                        .request_permissions(RuntimePermissionRequest {
                            id: invocation.request.id.clone(),
                            reason: Some("grant sibling access".to_string()),
                            permissions: RequestPermissionProfile {
                                file_system: Some(RequestFileSystemPermissions {
                                    read: None,
                                    write: Some(vec![PathBuf::from("/sibling-grant")]),
                                    entries: None,
                                }),
                                network: None,
                            },
                            context:
                                crate::runtime_permission::RuntimePermissionContext::foreground(
                                    crate::surface::SurfacePermissionOrigin::Unknown,
                                ),
                        })
                        .expect("grant first normal call");
                    tool_types::ToolResult::completed(
                        &invocation.request,
                        "granted".to_string(),
                        false,
                    )
                }
                1 if invocation
                    .permission_overlay
                    .additional_working_directories()
                    .contains(&PathBuf::from("/sibling-grant")) =>
                {
                    tool_types::ToolResult::completed(
                        &invocation.request,
                        "observed sibling grant".to_string(),
                        false,
                    )
                }
                _ => tool_types::ToolResult::failed(
                    &invocation.request,
                    "next normal call did not observe the turn grant",
                    None,
                ),
            }
        }
    }

    struct AllowTurnPermission;

    impl RuntimePermissionRequestHandler for AllowTurnPermission {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: false,
            })
        }
    }

    fn config(subagents: SubagentConfig) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
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
            subagents,
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn terminal_completion_is_injected_once_before_the_next_turn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = config(SubagentConfig::default());
        config.cwd = Some(temp.path().to_path_buf());
        let mut thread = RuntimeThread::start(&config, "terminal notification").expect("thread");
        let extensions = thread.thread_extensions_handle();
        let service = extensions
            .get_or_init(|| TerminalService::new(thread.session().task_registry().clone()));
        let overlay = crate::lifecycle::TurnPermissionOverlay::default();
        let started = service
            .exec(
                crate::terminal_service::TerminalExecRequest {
                    command: "sleep 0.1; printf notified",
                    cwd: temp.path(),
                    additional_roots: &[],
                    config: &config,
                    permission_overlay: &overlay,
                    terminal: crate::shell_session::ShellTerminalMode::pipe(),
                    sandbox_override: Some(
                        crate::shell_session::ShellSandboxMode::DangerFullAccess,
                    ),
                },
                Duration::from_millis(10),
                8 * 1024,
                || false,
            )
            .expect("start background terminal");
        assert_eq!(started.status, "running", "{started:?}");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let status = thread
                .session()
                .task_registry()
                .get(&started.task_id)
                .map(|record| record.status);
            if status == Some(orca_core::task_types::TaskStatus::Completed) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "observed {status:?}");
            std::thread::sleep(Duration::from_millis(10));
        }

        drain_terminal_notifications(thread.session_mut(), Some(extensions.as_ref()));
        let message_count = thread.session().conversation().messages.len();
        assert!(matches!(
            thread.session().conversation().messages.last(),
            Some(Message::System { content, pinned: true })
                if content.contains("<task-notification>")
                    && content.contains("notified")
                    && content.contains(&started.task_id)
        ));

        drain_terminal_notifications(thread.session_mut(), Some(extensions.as_ref()));
        assert_eq!(
            thread.session().conversation().messages.len(),
            message_count
        );
    }

    #[test]
    fn headless_controller_propagates_borrowed_writer_failure() {
        struct BrokenWriter;

        impl io::Write for BrokenWriter {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "borrowed writer disconnected",
                ))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut config = config(SubagentConfig::default());
        config.prompt = "inspect repo".to_string();
        config.output_format = OutputFormat::Jsonl;
        let error = run_inner(config, BrokenWriter, ControllerRunOptions::default(), None)
            .expect_err("borrowed writer failure");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "borrowed writer disconnected");
    }

    #[test]
    fn headless_controller_emits_one_contiguous_session_lifecycle() {
        let mut config = config(SubagentConfig::default());
        config.prompt = "inspect repo".to_string();
        config.output_format = OutputFormat::Jsonl;
        let mut output = Vec::new();

        let status = run_inner(config, &mut output, ControllerRunOptions::default(), None)
            .expect("headless controller run");

        assert_eq!(status, RunStatus::Success.exit_code());
        let events = String::from_utf8(output)
            .expect("utf8 events")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json event"))
            .collect::<Vec<_>>();
        assert_eq!(events.first().unwrap()["type"], "session.started");
        assert_eq!(events.last().unwrap()["type"], "session.completed");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "session.started")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "session.completed")
                .count(),
            1
        );
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], sequence);
            assert_eq!(event["run_id"], events[0]["run_id"]);
        }
    }

    #[test]
    fn root_turn_refreshes_plan_mode_context_after_runtime_mode_changes() {
        let mut config = config(SubagentConfig::default());
        config.history_mode = HistoryMode::Disabled;
        let mut thread = RuntimeThread::start(&config, "dynamic plan context").expect("thread");

        config.approval_mode = ApprovalMode::Plan;
        thread
            .run_request(
                &config,
                &ThreadTurnRequest::new("inspect the implementation"),
                Vec::new(),
            )
            .expect("plan turn");
        let plan_context = thread
            .session()
            .conversation()
            .internal_context
            .get(orca_core::conversation::MODE_CONTEXT_FRAGMENT_ID)
            .expect("plan mode context");
        assert!(plan_context.content.contains("[Mode context]"));
        assert!(
            plan_context
                .content
                .contains("exactly one `<proposed_plan>` block")
        );

        config.approval_mode = ApprovalMode::AutoEdit;
        thread
            .run_request(
                &config,
                &ThreadTurnRequest::new("implement the approved plan"),
                Vec::new(),
            )
            .expect("implementation turn");
        assert!(
            thread
                .session()
                .conversation()
                .internal_context
                .get(orca_core::conversation::MODE_CONTEXT_FRAGMENT_ID)
                .is_none()
        );
    }

    fn assert_controller_failure_persists_error(use_event_factory: bool) {
        let mut config = config(SubagentConfig::default());
        config.history_mode = HistoryMode::Record;
        config.output_format = OutputFormat::Jsonl;
        let mut thread = RuntimeThread::start(&config, "provider failure").expect("thread");
        let thread_id = thread.thread_id().to_string();
        let request = ThreadTurnRequest::new("mock_provider_error");
        let mut output = Vec::new();

        let status = if use_event_factory {
            let mut events = EventFactory::new(thread_id.clone());
            thread.run_request_with_event_factory(&config, &request, &mut output, &mut events)
        } else {
            thread.run_request(&config, &request, &mut output)
        }
        .expect("provider failure completes the turn");

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(
            thread.session().completion_error(),
            Some("mock provider error: api_key=super-secret")
        );
        let transcript =
            crate::history::load_session(&thread_id).expect("failed session transcript");
        assert_eq!(
            transcript.completion_error.as_deref(),
            Some("mock provider error: api_key=<redacted>")
        );
        let persisted = std::fs::read_to_string(&transcript.path).expect("session JSONL");
        assert!(!persisted.contains("super-secret"));
    }

    #[test]
    fn controller_default_path_persists_redacted_provider_error() {
        assert_controller_failure_persists_error(false);
    }

    #[test]
    fn controller_event_factory_path_persists_redacted_provider_error() {
        assert_controller_failure_persists_error(true);
    }

    #[test]
    fn hosted_turn_persists_one_user_record_for_one_admitted_prompt() {
        let mut config = config(SubagentConfig::default());
        config.history_mode = HistoryMode::Record;
        config.output_format = OutputFormat::Jsonl;
        let mut thread = RuntimeThread::start(&config, "canonical user turn").expect("thread");
        let request = ThreadTurnRequest::new("persist this prompt once");
        let turn_id = request.turn_id().clone();

        thread
            .run_request(&config, &request, Vec::new())
            .expect("run hosted turn");

        let records = thread
            .session()
            .conversation_records()
            .expect("recorded conversation ledger");
        let projected = crate::thread_store::conversation_records_to_thread_items(
            thread.thread_id(),
            &records,
            None,
            usize::MAX,
        )
        .expect("project recorded conversation ledger");
        let user_records = projected
            .iter()
            .filter(|record| {
                record.item["role"] == "user"
                    && record.item["content"] == "persist this prompt once"
            })
            .collect::<Vec<_>>();

        assert_eq!(
            user_records.len(),
            1,
            "one admitted prompt must have one durable user item: {user_records:#?}"
        );
        assert_eq!(user_records[0].turn_id, turn_id.as_str());
        assert!(user_records[0].item_id.starts_with("item_"));
    }

    #[test]
    fn completed_persistent_root_turn_records_automatic_memory_with_provenance() {
        crate::history::with_redirected_orca_home("auto-memory-completed-root", |home| {
            let cwd = tempfile::tempdir().expect("cwd");
            let mut config = config(SubagentConfig::default());
            config.cwd = Some(cwd.path().to_path_buf());
            config.history_mode = HistoryMode::Record;
            config.auto_memory = true;
            let mut thread = RuntimeThread::start(&config, "automatic memory").expect("thread");
            let session_id = thread.thread_id().to_string();
            let request = ThreadTurnRequest::new(
                "Remember that release qualification requires a clean install smoke test.",
            );
            let turn_id = request.turn_id().to_string();

            let status = thread
                .run_request(&config, &request, Vec::new())
                .expect("successful root turn");

            assert_eq!(status, RunStatus::Success);
            thread.session().wait_for_automatic_memory();
            let candidates = automatic_memory_candidate_files(home);
            assert_eq!(candidates.len(), 1);
            let records = std::fs::read_to_string(&candidates[0]).expect("candidate ledger");
            let candidate: serde_json::Value =
                serde_json::from_str(records.lines().next().expect("candidate")).expect("json");
            assert_eq!(candidate["turn_id"], turn_id);
            assert_eq!(candidate["session_id"], session_id);

            let next_status = thread
                .run_request(
                    &config,
                    &ThreadTurnRequest::new(
                        "What clean install smoke test is required for release qualification?",
                    ),
                    Vec::new(),
                )
                .expect("next turn recalls memory");
            assert_eq!(next_status, RunStatus::Success);
            let recalled = thread
                .session()
                .conversation()
                .internal_context
                .get(orca_core::conversation::MEMORY_CONTEXT_FRAGMENT_ID)
                .expect("recalled memory context");
            assert!(recalled.content.contains("clean install smoke test"));
            assert!(recalled.content.contains(&turn_id));
        });
    }

    #[test]
    fn verification_failed_turn_does_not_record_automatic_memory() {
        crate::history::with_redirected_orca_home("auto-memory-verifier-failed", |home| {
            let cwd = tempfile::tempdir().expect("cwd");
            let mut config = config(SubagentConfig::default());
            config.cwd = Some(cwd.path().to_path_buf());
            config.history_mode = HistoryMode::Record;
            config.auto_memory = true;
            config.verifier = Some("exit 9".to_string());
            let mut thread =
                RuntimeThread::start(&config, "failed automatic memory").expect("thread");

            let status = thread
                .run_request(
                    &config,
                    &ThreadTurnRequest::new("This turn must fail verification."),
                    Vec::new(),
                )
                .expect("verified root turn");

            assert_eq!(status, RunStatus::VerificationFailed);
            assert!(automatic_memory_candidate_files(home).is_empty());
        });
    }

    #[test]
    fn history_disabled_turn_neither_recalls_nor_writes_automatic_memory() {
        crate::history::with_redirected_orca_home("auto-memory-stateless", |home| {
            let cwd = tempfile::tempdir().expect("cwd");
            let mut config = config(SubagentConfig::default());
            config.cwd = Some(cwd.path().to_path_buf());
            config.history_mode = HistoryMode::Disabled;
            config.auto_memory = true;
            let mut thread =
                RuntimeThread::start(&config, "stateless automatic memory").expect("thread");

            let status = thread
                .run_request(
                    &config,
                    &ThreadTurnRequest::new("A stateless turn must not use memory."),
                    Vec::new(),
                )
                .expect("stateless root turn");

            assert_eq!(status, RunStatus::Success);
            assert!(!home.join("memory").exists());
        });
    }

    fn automatic_memory_candidate_files(home: &Path) -> Vec<PathBuf> {
        let projects = home.join("memory/projects");
        let Ok(entries) = std::fs::read_dir(projects) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("candidates.jsonl"))
            .filter(|path| path.is_file())
            .collect()
    }

    #[test]
    fn hosted_turn_does_not_commit_user_prompt_when_history_append_fails() {
        // This test replaces the transcript directory with a file, which
        // would corrupt the shared process-wide home; it needs the redirect
        // to a private home. The turn executes on this thread, so holding the
        // exclusive env lock cannot deadlock any background host.
        crate::history::with_redirected_orca_home("failed-user-admission", |_| {
            let mut config = config(SubagentConfig::default());
            config.history_mode = HistoryMode::Record;
            config.output_format = OutputFormat::Jsonl;
            let mut thread =
                RuntimeThread::start(&config, "failed user admission").expect("thread");
            let transcript = crate::history::load_session(thread.thread_id()).expect("transcript");
            let transcript_dir = transcript.path.parent().expect("transcript directory");
            std::fs::remove_dir_all(transcript_dir).expect("remove transcript directory");
            std::fs::write(transcript_dir, "block transcript directory recreation")
                .expect("replace transcript directory with file");

            thread
                .run_request(
                    &config,
                    &ThreadTurnRequest::new("must not enter model context"),
                    Vec::new(),
                )
                .expect_err("missing transcript directory must reject turn admission");

            assert!(
                thread
                    .session()
                    .conversation()
                    .messages
                    .iter()
                    .all(|message| {
                        !matches!(
                            message,
                            Message::User { content, .. }
                                if content == "must not enter model context"
                        )
                    }),
                "a failed durable append must not commit the prompt to model context"
            );
            let records = thread
                .session()
                .conversation_records()
                .expect("conversation ledger");
            let projected = crate::thread_store::conversation_records_to_thread_items(
                thread.thread_id(),
                &records,
                None,
                usize::MAX,
            )
            .expect("project conversation ledger");
            assert!(
                projected.iter().all(|record| {
                    record.item["role"] != "user"
                        || record.item["content"] != "must not enter model context"
                }),
                "a failed durable append must not enter the live projection ledger: {projected:#?}"
            );
        });
    }

    fn subagent_request(id: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("task".to_string()),
            raw_arguments: None,
        }
    }

    fn tool_request(
        id: &str,
        name: tool_types::ToolName,
        action: ActionKind,
    ) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name,
            action,
            target: Some("target".to_string()),
            raw_arguments: None,
        }
    }

    fn tool_continuation(request: tool_types::ToolRequest) -> RuntimeTurnContinuation {
        let raw_tool_call = orca_core::conversation::RawToolCall {
            id: request.id.clone(),
            function_name: request.name.as_str().to_string(),
            arguments: request
                .raw_arguments
                .clone()
                .unwrap_or_else(|| "{}".to_string()),
        };
        RuntimeTurnContinuation::from_response(
            orca_core::provider_types::ProviderResponse {
                steps: vec![orca_core::provider_types::ProviderStep::ToolCall(request)],
                assistant_content: Some("Executing the requested tool.".to_string()),
                assistant_reasoning: None,
                tool_calls: vec![raw_tool_call],
                usage: None,
            },
            orca_core::thread_identity::TurnId::new(),
        )
    }

    #[test]
    fn hosted_goal_tool_rejects_unverified_terminal_update_through_runtime_context() {
        // The goal created directly on the shared process-wide store would be
        // recovered as interrupted by a concurrent opener; keep a private
        // goal store for the duration (a thread-local override, invisible to
        // other tests). The turn executes on this thread, so the override is
        // visible to every goal store read it performs.
        let _home = crate::history::redirect_test_orca_home(
            &crate::history::isolated_test_orca_home_subdir("hosted-goal-private"),
        );
        let mut config = config(SubagentConfig::default());
        config.history_mode = HistoryMode::Record;
        config.output_format = OutputFormat::Jsonl;
        config.approval_mode = ApprovalMode::FullAuto;
        let mut thread = RuntimeThread::start(&config, "hosted goal tool").expect("thread");
        let session_id = thread
            .session()
            .session_id()
            .expect("recorded session id")
            .to_string();
        let store = crate::goal_store::GoalStore::load_default().expect("goal store");
        store
            .create_goal(crate::goal_store::CreateGoalInput {
                session_id: session_id.clone(),
                objective: "finish the hosted goal".to_string(),
                token_budget: None,
                now: 1,
            })
            .expect("active goal");

        let progress = ThreadTurnRequest::new("establish live goal progress")
            .with_tool_mode(ThreadTurnToolMode::Goal)
            .with_continuation(tool_continuation(tool_types::ToolRequest {
                id: "goal-progress-1".to_string(),
                name: tool_types::ToolName::TaskList,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            }));
        assert_eq!(
            thread
                .run_request(&config, &progress, Vec::new())
                .expect("run non-goal progress tool"),
            RunStatus::Success
        );

        let update = ThreadTurnRequest::new("complete the hosted goal")
            .with_tool_mode(ThreadTurnToolMode::Goal)
            .with_continuation(tool_continuation(tool_types::ToolRequest {
                id: "goal-update-1".to_string(),
                name: tool_types::ToolName::UpdateGoal,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"status":"complete"}"#.to_string()),
            }));
        let status = thread
            .run_request(&config, &update, Vec::new())
            .expect("run hosted goal update");

        assert_eq!(
            status,
            RunStatus::Success,
            "hosted goal messages: {:#?}",
            thread.session().conversation().messages
        );
        assert_eq!(
            store
                .project_thread_goal(&session_id)
                .expect("load persisted goal")
                .expect("persisted goal")
                .status,
            orca_core::goal_types::ThreadGoalStatus::Active
        );
        assert!(
            thread
                .session()
                .conversation()
                .messages
                .iter()
                .any(|message| {
                    matches!(
                        message,
                        Message::Tool {
                            tool_call_id,
                            terminal: Some(terminal),
                            ..
                        } if tool_call_id == "goal-update-1"
                            && terminal.status == tool_types::ToolStatus::Failed
                    )
                })
        );
    }

    #[test]
    fn hosted_goal_tool_without_persistent_context_stops_failed_turn() {
        let mut config = config(SubagentConfig::default());
        config.output_format = OutputFormat::Jsonl;
        config.approval_mode = ApprovalMode::FullAuto;
        let mut thread = RuntimeThread::start(&config, "missing goal context").expect("thread");
        assert!(thread.session().session_id().is_none());
        let request = ThreadTurnRequest::new("complete unavailable goal")
            .with_tool_mode(ThreadTurnToolMode::Goal)
            .with_continuation(tool_continuation(tool_types::ToolRequest {
                id: "goal-update-missing-context".to_string(),
                name: tool_types::ToolName::UpdateGoal,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"status":"complete"}"#.to_string()),
            }));

        let status = thread
            .run_request(&config, &request, Vec::new())
            .expect("goal control failure completes the hosted turn");
        let messages = &thread.session().conversation().messages;
        let matching_results = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                Message::Tool {
                    tool_call_id,
                    terminal: Some(terminal),
                    ..
                } if tool_call_id == "goal-update-missing-context" => Some((index, terminal)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            status,
            RunStatus::Failed,
            "missing-context messages: {messages:#?}"
        );
        assert_eq!(matching_results.len(), 1);
        assert_eq!(matching_results[0].1.status, tool_types::ToolStatus::Failed);
        assert!(
            messages[matching_results[0].0 + 1..]
                .iter()
                .all(|message| !matches!(message, Message::Assistant { .. })),
            "goal control failure must not resume model sampling: {messages:#?}"
        );
    }

    #[test]
    fn hosted_goal_tool_invalid_update_remains_model_recoverable() {
        // The goal created directly on the shared process-wide store would be
        // recovered as interrupted by a concurrent opener; keep a private
        // goal store for the duration (a thread-local override, invisible to
        // other tests). The turn executes on this thread, so the override is
        // visible to every goal store read it performs.
        let _home = crate::history::redirect_test_orca_home(
            &crate::history::isolated_test_orca_home_subdir("hosted-goal-private"),
        );
        let mut config = config(SubagentConfig::default());
        config.history_mode = HistoryMode::Record;
        config.output_format = OutputFormat::Jsonl;
        config.approval_mode = ApprovalMode::FullAuto;
        let mut thread = RuntimeThread::start(&config, "invalid goal update").expect("thread");
        let session_id = thread.session().session_id().unwrap().to_string();
        let store = crate::goal_store::GoalStore::load_default().unwrap();
        store
            .create_goal(crate::goal_store::CreateGoalInput {
                session_id: session_id.clone(),
                objective: "keep correcting the goal update".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();
        let request = ThreadTurnRequest::new("reject invalid goal update")
            .with_tool_mode(ThreadTurnToolMode::Goal)
            .with_continuation(tool_continuation(tool_types::ToolRequest {
                id: "goal-update-invalid".to_string(),
                name: tool_types::ToolName::UpdateGoal,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"status":"paused"}"#.to_string()),
            }));

        let status = thread
            .run_request(&config, &request, Vec::new())
            .expect("run invalid goal update");
        let messages = &thread.session().conversation().messages;
        let result_index = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    Message::Tool {
                        tool_call_id,
                        terminal: Some(terminal),
                        ..
                    } if tool_call_id == "goal-update-invalid"
                        && terminal.status == tool_types::ToolStatus::Failed
                )
            })
            .expect("failed goal tool result");

        assert_eq!(status, RunStatus::Success);
        assert_eq!(
            store
                .project_thread_goal(&session_id)
                .unwrap()
                .unwrap()
                .status,
            orca_core::goal_types::ThreadGoalStatus::Active
        );
        assert!(
            messages[result_index + 1..]
                .iter()
                .any(|message| matches!(message, Message::Assistant { .. })),
            "invalid goal arguments must allow another model sample: {messages:#?}"
        );
    }

    #[test]
    fn hosted_goal_tool_store_failure_stops_failed_turn() {
        // The broken legacy goal fixture must be visible to the goal store
        // through `ORCA_HOME` itself, so this test redirects the variable
        // under the exclusive env lock. The turn executes on this thread, so
        // the exclusive lock cannot deadlock any background host.
        crate::history::with_redirected_orca_home("broken-goal-store", |home| {
            let mut config = config(SubagentConfig::default());
            config.history_mode = HistoryMode::Record;
            config.output_format = OutputFormat::Jsonl;
            config.approval_mode = ApprovalMode::FullAuto;
            let mut thread = RuntimeThread::start(&config, "broken goal store").expect("thread");
            std::fs::write(home.join("goals_1.json"), "{not valid JSON")
                .expect("break goal store fixture");
            let request = ThreadTurnRequest::new("read unavailable goal store")
                .with_tool_mode(ThreadTurnToolMode::Goal)
                .with_continuation(tool_continuation(tool_types::ToolRequest {
                    id: "goal-get-store-failure".to_string(),
                    name: tool_types::ToolName::GetGoal,
                    action: ActionKind::Read,
                    target: None,
                    raw_arguments: Some("{}".to_string()),
                }));

            let status = thread
                .run_request(&config, &request, Vec::new())
                .expect("goal store failure completes turn");
            let messages = &thread.session().conversation().messages;
            let matching_results = messages
                .iter()
                .enumerate()
                .filter(|(_, message)| {
                    matches!(
                        message,
                        Message::Tool {
                            tool_call_id,
                            terminal: Some(terminal),
                            ..
                        } if tool_call_id == "goal-get-store-failure"
                            && terminal.status == tool_types::ToolStatus::Failed
                    )
                })
                .collect::<Vec<_>>();

            assert_eq!(status, RunStatus::Failed);
            assert_eq!(matching_results.len(), 1);
            assert!(
                messages[matching_results[0].0 + 1..]
                    .iter()
                    .all(|message| !matches!(message, Message::Assistant { .. })),
                "goal store failure must not resume model sampling: {messages:#?}"
            );
        });
    }

    #[test]
    fn thread_turn_request_routes_user_input_handler_through_agent_loop() {
        struct AnswerHandler;

        impl RuntimeUserInputHandler for AnswerHandler {
            fn request_user_input(
                &self,
                request: &RuntimeUserInputRequest,
            ) -> io::Result<Option<String>> {
                assert_eq!(request.question, "Confirm: Continue?");
                assert_eq!(request.choices, ["yes - Continue", "no - Stop"]);
                Ok(Some("yes".to_string()))
            }
        }

        let mut config = config(SubagentConfig::default());
        config.output_format = OutputFormat::Jsonl;
        config.approval_mode = ApprovalMode::FullAuto;
        let mut thread = RuntimeThread::start(&config, "user input turn").expect("thread");
        let request = ThreadTurnRequest::new("ask Continue?")
            .with_user_input_handler(Arc::new(AnswerHandler));

        let status = thread
            .run_request(&config, &request, Vec::new())
            .expect("run request");

        assert_eq!(status, RunStatus::Success);
        assert!(
            thread
                .session()
                .conversation()
                .messages
                .iter()
                .any(|message| {
                    matches!(
                        message,
                        Message::Tool { content, .. }
                            if content == r#"{"answers":{"Continue?":"yes"}}"#
                    )
                })
        );
    }

    #[test]
    fn thread_turn_request_continuation_does_not_append_user_prompt() {
        let mut config = config(SubagentConfig::default());
        config.output_format = OutputFormat::Jsonl;
        let mut thread = RuntimeThread::start(&config, "continuation turn").expect("thread");
        let response = orca_core::provider_types::ProviderResponse {
            steps: vec![orca_core::provider_types::ProviderStep::MessageDelta(
                "continued".to_string(),
            )],
            assistant_content: Some("continued".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let request = ThreadTurnRequest::new("resume marker").with_continuation(
            crate::background_turn::RuntimeTurnContinuation::from_response(
                response,
                orca_core::thread_identity::TurnId::new(),
            ),
        );

        let status = thread
            .run_request(&config, &request, Vec::new())
            .expect("run continuation request");

        assert_eq!(status, RunStatus::Success);
        assert!(
            thread
                .session()
                .conversation()
                .messages
                .iter()
                .all(|message| {
                    !matches!(message, Message::User { content, .. } if content == "resume marker")
                }),
            "continuation requests must not append a fresh user prompt"
        );
        assert!(
            thread.session().conversation().messages.iter().any(|message| {
                matches!(message, Message::Assistant { content, .. } if content.as_deref() == Some("continued"))
            })
        );
    }

    #[test]
    fn existing_turn_request_does_not_append_user_prompt_again() {
        let mut config = config(SubagentConfig::default());
        config.output_format = OutputFormat::Jsonl;
        let mut thread = RuntimeThread::start(&config, "existing turn").expect("thread");
        let initial = ThreadTurnRequest::new("original prompt");
        thread
            .run_request(&config, &initial, Vec::new())
            .expect("run initial generation");
        let original_count = thread
            .session()
            .conversation()
            .messages
            .iter()
            .filter(|message| {
                matches!(message, Message::User { content, .. } if content == "original prompt")
            })
            .count();

        let resumed = ThreadTurnRequest::new("original prompt").with_existing_turn_prompt();
        thread
            .run_request(&config, &resumed, Vec::new())
            .expect("run resumed generation");
        let resumed_count = thread
            .session()
            .conversation()
            .messages
            .iter()
            .filter(|message| {
                matches!(message, Message::User { content, .. } if content == "original prompt")
            })
            .count();

        assert_eq!(original_count, 1);
        assert_eq!(resumed_count, 1);
    }

    #[test]
    fn workflow_ipc_tool_requires_workflow_child_context() {
        let mut context = RuntimeToolActorContext::new("test-run");
        let request = tool_types::ToolRequest {
            id: "mailbox".to_string(),
            name: tool_types::ToolName::WorkflowReadMessages,
            action: ActionKind::Agent,
            target: Some("findings".to_string()),
            raw_arguments: Some(serde_json::json!({ "channel": "findings" }).to_string()),
        };

        let result = context.execute_workflow_ipc_tool(&request, None);

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("only available inside workflow child agents")
        );
    }

    #[test]
    fn subagent_batch_respects_parallel_limit() {
        let config = config(SubagentConfig::default());
        let requests = vec![
            subagent_request("a"),
            subagent_request("b"),
            subagent_request("c"),
            subagent_request("d"),
            subagent_request("e"),
            subagent_request("f"),
            subagent_request("g"),
        ];

        assert!(should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 0), 6);
    }

    #[test]
    fn async_subagent_skips_sync_batch_path() {
        let config = config(SubagentConfig::default());
        let request = tool_types::ToolRequest {
            id: "async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("async task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        assert!(!should_run_subagent_batch(&config, &request, 0));
    }

    #[test]
    fn max_parallel_one_uses_sequential_subagent_path() {
        let config = config(
            SubagentConfig {
                max_depth: 2,
                max_parallel: 1,
                ..SubagentConfig::default()
            }
            .normalized(),
        );
        let request = subagent_request("a");

        assert!(!should_run_subagent_batch(&config, &request, 0));
    }

    #[test]
    fn subagent_batch_stops_at_first_non_subagent_tool() {
        let config = config(SubagentConfig::default());
        let mut requests = vec![subagent_request("a"), subagent_request("b")];
        requests.push(tool_types::ToolRequest {
            id: "read".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("src/main.rs".to_string()),
            raw_arguments: None,
        });
        requests.push(subagent_request("c"));

        assert_eq!(collect_subagent_batch(&config, &requests, 0), 2);
    }

    #[test]
    fn subagent_batch_stops_at_first_async_subagent() {
        let config = config(SubagentConfig::default());
        let async_request = tool_types::ToolRequest {
            id: "async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("async task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let requests = vec![subagent_request("a"), async_request, subagent_request("b")];

        assert_eq!(collect_subagent_batch(&config, &requests, 0), 1);
    }

    #[test]
    fn subagent_status_returns_session_local_task_result() {
        let mut context = RuntimeToolActorContext::new("test-run");
        let registry = TaskRegistry::new("session-status".to_string());
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry
            .complete(&task.id, "finished async audit".to_string())
            .unwrap();
        let request = tool_types::ToolRequest {
            id: "status".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "agent_id": task.id }).to_string()),
        };

        let result = context.execute_subagent_status_tool(&request, &registry);

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let payload: serde_json::Value =
            serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["description"], "inspect auth");
        assert_eq!(payload["agent_type"], "general");
        assert!(payload["created_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["started_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["completed_at_ms"].as_i64().unwrap() > 0);
        assert_eq!(payload["output"], "finished async audit");
        assert_eq!(payload["error"], serde_json::Value::Null);
    }

    #[test]
    fn subagent_status_pages_persisted_result_output() {
        let mut context = RuntimeToolActorContext::new("test-run");
        let registry = TaskRegistry::new("session-status-page".to_string());
        let task = registry.create_subagent("inspect large output".to_string(), None);
        registry
            .complete(&task.id, "0123456789".to_string())
            .unwrap();
        let request = tool_types::ToolRequest {
            id: "status-page".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "agent_id": task.id,
                    "offset": 4,
                    "limit": 3,
                })
                .to_string(),
            ),
        };

        let result = context.execute_subagent_status_tool(&request, &registry);

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let payload: serde_json::Value =
            serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        assert_eq!(payload["output"], "456");
        assert_eq!(payload["output_total_chars"], 10);
        assert_eq!(payload["output_offset"], 4);
        assert_eq!(payload["output_next_offset"], 7);
    }

    #[test]
    fn subagent_status_rejects_invalid_result_page_size() {
        let mut context = RuntimeToolActorContext::new("test-run");
        let registry = TaskRegistry::new("session-status-page-limit".to_string());
        let task = registry.create_subagent("inspect output".to_string(), None);
        registry.complete(&task.id, "result".to_string()).unwrap();
        let request = tool_types::ToolRequest {
            id: "status-page-limit".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "agent_id": task.id,
                    "limit": 0,
                })
                .to_string(),
            ),
        };

        let result = context.execute_subagent_status_tool(&request, &registry);

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.terminal().kind,
            tool_types::ToolResultKind::InvalidInput
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("between 1 and 32000")
        );
    }

    #[test]
    fn readonly_batch_respects_parallel_limit() {
        let mut config = config(SubagentConfig::default());
        config.tools.max_read_parallel = 2;
        let requests = vec![
            tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Read),
            tool_request("b", tool_types::ToolName::Grep, ActionKind::Read),
            tool_request("c", tool_types::ToolName::ListFiles, ActionKind::Read),
        ];

        assert!(orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &requests[0]
        ));
        assert_eq!(
            orca_tools::collect_readonly_batch(config.tools.max_read_parallel, &requests, 0),
            2
        );
    }

    #[test]
    fn readonly_batch_stops_at_first_mutating_tool() {
        let config = config(SubagentConfig::default());
        let requests = vec![
            tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Read),
            tool_request("b", tool_types::ToolName::Bash, ActionKind::Shell),
            tool_request("c", tool_types::ToolName::Grep, ActionKind::Read),
        ];

        assert_eq!(
            orca_tools::collect_readonly_batch(config.tools.max_read_parallel, &requests, 0),
            1
        );
        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &requests[1]
        ));
    }

    #[test]
    fn readonly_batch_uses_spec_not_request_action() {
        let config = config(SubagentConfig::default());
        let request = tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Write);

        assert!(orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn readonly_batch_rejects_shell_by_capability() {
        let config = config(SubagentConfig::default());
        let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);

        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn approval_action_rejects_caller_supplied_read_for_shell() {
        let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);
        let registry = McpRegistry::default();

        assert_eq!(
            canonical_action_for_tool(&request, &registry, &[]),
            ActionKind::Shell
        );
    }

    #[test]
    fn readonly_batch_skips_network_actions() {
        let config = config(SubagentConfig::default());
        let request = tool_request("a", tool_types::ToolName::WebSearch, ActionKind::Network);

        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn readonly_batch_executes_results_in_request_order() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bravo").unwrap();
        let requests = vec![
            tool_types::ToolRequest {
                target: Some("a.txt".to_string()),
                raw_arguments: Some(r#"{"path":"a.txt"}"#.to_string()),
                ..tool_request("first", tool_types::ToolName::ReadFile, ActionKind::Read)
            },
            tool_types::ToolRequest {
                target: Some("b.txt".to_string()),
                raw_arguments: Some(r#"{"path":"b.txt"}"#.to_string()),
                ..tool_request("second", tool_types::ToolName::ReadFile, ActionKind::Read)
            },
        ];
        let mut events = EventFactory::new("test-run".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let cancel = CancelToken::new();
        let results = crate::runtime_readonly_tool_turn::execute_readonly_batch(
            crate::runtime_readonly_tool_turn::RuntimeReadonlyBatchContext {
                cwd: dir.path(),
                events: &mut events,
                sink: &mut sink,
                tool_requests: &requests,
                emit_deltas: true,
                mcp_registry: &registry,
                hooks: &hooks,
                cancel: &cancel,
                output_truncation: tool_types::ToolOutputTruncation::default(),
                max_parallel: 2,
                provider_response_ingress: None,
            },
        )
        .unwrap()
        .results;

        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(results[0].output.as_deref(), Some("alpha"));
        assert_eq!(results[1].output.as_deref(), Some("bravo"));
    }

    #[test]
    fn pre_model_hook_context_is_added_as_pinned_system_message() {
        let mut conversation = Conversation::new();
        conversation.add_system("base system".to_string());
        conversation.add_user("do work".to_string());
        let outcome = HookOutcome {
            modified_target: None,
            injected_context: vec!["policy hint".to_string(), "repo hint".to_string()],
        };

        let model_conversation = conversation_with_hook_context(&conversation, &outcome);

        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(model_conversation.messages.len(), 3);
        assert!(matches!(
            model_conversation.messages.last(),
            Some(orca_core::conversation::Message::System { content, pinned: true })
                if content.contains("policy hint") && content.contains("repo hint")
        ));
    }

    #[test]
    fn agent_tool_policy_context_groups_child_tool_policy() {
        let allowed_tools = vec!["read".to_string(), "edit".to_string()];
        let context =
            AgentToolPolicyContext::new(Some(allowed_tools.as_slice()), Some("review-only"));

        assert_eq!(context.allowed_tools().unwrap(), allowed_tools.as_slice());
        assert_eq!(context.label(), Some("review-only"));
    }

    #[test]
    fn tool_execution_context_groups_tool_services() {
        let cwd = std::env::temp_dir().join("orca-tool-execution-services");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);

        let context = ToolExecutionContext::new(&cwd, 1, true, &policy).with_services(
            &instructions,
            &memory,
            &registry,
            &hooks,
        );

        assert_eq!(context.cwd(), cwd.as_path());
        assert_eq!(context.subagent_depth(), 1);
        assert!(context.emit_deltas());
        assert!(std::ptr::eq(context.policy(), &policy));
        assert!(std::ptr::eq(context.instructions(), &instructions));
        assert!(std::ptr::eq(context.memory(), &memory));
        assert!(std::ptr::eq(context.mcp_registry(), &registry));
        assert!(std::ptr::eq(context.hooks(), &hooks));
    }

    #[test]
    fn tool_execution_context_groups_runtime_state() {
        let cwd = std::env::temp_dir().join("orca-tool-execution-runtime");
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-execution-runtime".to_string());
        let mut background_workflows = Vec::new();

        let context = ToolExecutionContext::new(&cwd, 0, false, &policy).with_runtime(
            &mut cost_tracker,
            &cancel,
            &task_registry,
            &mut background_workflows,
            None,
        );

        assert_eq!(context.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(context.cancel(), &cancel));
        assert!(std::ptr::eq(context.task_registry(), &task_registry));
        assert_eq!(context.background_workflow_count(), 0);
        assert!(context.workflow_ipc().is_none());
    }

    #[test]
    fn tool_execution_actor_owns_runtime_tool_actor_state() {
        let actor = ToolExecutionActor::new("tool-actor-run");
        let task = actor.active_task().expect("active task");

        assert_eq!(task.kind(), RuntimeTaskKind::Agent);
        assert_eq!(task.status(), RuntimeTaskStatus::Running);
    }

    #[test]
    fn tool_execution_actor_executes_normal_tool_from_context() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").unwrap();
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("tool-actor-execute".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            id: "read-file".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-actor-execute".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = crate::lifecycle::TurnPermissionOverlay::default();
        let context = ToolExecutionContext::new(cwd.path(), 0, true, &policy)
            .with_services(&instructions, &memory, &registry, &hooks)
            .with_runtime(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
            )
            .with_permission_overlay(&mut permission_overlay);

        let mut actor = ToolExecutionActor::new(events.run_id().to_string());
        let (status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                context,
                execute_child_agent_loop,
                execute_child_agent_loop,
            )
            .unwrap();

        assert_eq!(status, RunStatus::Success);
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.id, "read-file");
    }

    #[test]
    fn tool_execution_actor_approval_allows_read_tool_to_continue() {
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("tool-actor-approval".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            id: "read-file".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let registry = McpRegistry::default();
        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut permission_overlay = crate::lifecycle::TurnPermissionOverlay::default();
        let cancel = CancelToken::new();

        let mut actor = ToolExecutionActor::new(events.run_id().to_string());
        let execution = actor.handle_approval(ToolApprovalGateContext {
            config: &config,
            events: &mut events,
            sink: &mut sink,
            tool_request: &request,
            invocation: &invocation,
            policy: &policy,
            permission_overlay: &mut permission_overlay,
            approval_handler: None,
            cancel: &cancel,
            emit_deltas: true,
            provider_response_ingress: None,
        });

        assert!(execution.outcome.is_none());
        assert!(execution.event_error.is_none());
    }

    #[test]
    fn runtime_tool_router_dispatches_normal_tool() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").unwrap();
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("tool-actor-dispatch".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            id: "read-file".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let registry = McpRegistry::default();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-actor-dispatch".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = crate::lifecycle::TurnPermissionOverlay::default();
        let mut event_error = None;

        let mut runtime = RuntimeToolActorContext::new(events.run_id().to_string());
        let result = RuntimeToolRouter::new(&mut runtime)
            .dispatch(RuntimeToolInvocationContext {
                config: &config,
                cwd: cwd.path(),
                events: &mut events,
                sink: &mut sink,
                execution_request: &request,
                goal_mode: false,
                subagent_depth: 0,
                instructions: &instructions,
                memory: &memory,
                mcp_registry: &registry,
                hooks: &hooks,
                emit_deltas: true,
                cost_tracker: &mut cost_tracker,
                cancel: &cancel,
                task_registry: &task_registry,
                background_workflows: &mut background_workflows,
                workflow_ipc: None,
                permission_overlay: &mut permission_overlay,
                permission_handler: None,
                user_input_handler: None,
                mcp_elicitation_handler: None,
                extension_stores: None,
                goal_runtime: None,
                goal_turn: None,
                event_error: &mut event_error,
                subagent_child_executor: execute_child_agent_loop,
                workflow_child_executor: execute_child_agent_loop,
                workflow_lifecycle_ingress: None,
                wait_for_background_workflows: true,
                root_task_id: None,
                child_budget: None,
            })
            .unwrap();

        assert_eq!(result.result.status, tool_types::ToolStatus::Completed);
        assert!(event_error.is_none());
    }

    #[test]
    fn runtime_tool_router_merges_normal_permission_delta_before_next_call() {
        let cwd = tempfile::tempdir().expect("cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("normal-permission-carry".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = ["grant", "observe"].map(|id| tool_types::ToolRequest {
            id: id.to_string(),
            name: tool_types::ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf unused".to_string()),
            raw_arguments: None,
        });
        let registry = McpRegistry::default();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("normal-permission-carry".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = crate::lifecycle::TurnPermissionOverlay::default();
        let permission_handler = AllowTurnPermission;
        let normal_handler = Arc::new(PermissionCarryNormalHandler {
            calls: AtomicUsize::new(0),
        });
        let mut runtime = RuntimeToolActorContext::new(events.run_id().to_string());

        for request in &requests {
            let tool_calls = RuntimeToolCallRuntime::with_normal_handler(normal_handler.clone())
                .expect("normal tool runtime");
            let mut event_error = None;
            let result = RuntimeToolRouter::with_tool_call_runtime(&mut runtime, tool_calls)
                .dispatch(RuntimeToolInvocationContext {
                    config: &config,
                    cwd: cwd.path(),
                    events: &mut events,
                    sink: &mut sink,
                    execution_request: request,
                    goal_mode: false,
                    subagent_depth: 0,
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &registry,
                    hooks: &hooks,
                    emit_deltas: false,
                    cost_tracker: &mut cost_tracker,
                    cancel: &cancel,
                    task_registry: &task_registry,
                    background_workflows: &mut background_workflows,
                    workflow_ipc: None,
                    permission_overlay: &mut permission_overlay,
                    permission_handler: Some(&permission_handler),
                    user_input_handler: None,
                    mcp_elicitation_handler: None,
                    extension_stores: None,
                    goal_runtime: None,
                    goal_turn: None,
                    event_error: &mut event_error,
                    subagent_child_executor: execute_child_agent_loop,
                    workflow_child_executor: execute_child_agent_loop,
                    workflow_lifecycle_ingress: None,
                    wait_for_background_workflows: true,
                    root_task_id: None,
                    child_budget: None,
                })
                .expect("dispatch normal call");

            assert_eq!(result.result.status, tool_types::ToolStatus::Completed);
            assert!(event_error.is_none());
        }

        assert_eq!(
            permission_overlay.additional_working_directories(),
            &[PathBuf::from("/sibling-grant")]
        );
        assert_eq!(normal_handler.calls.load(Ordering::Acquire), 2);
    }
}
