use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{
    InterruptSemantics, ReplaySemantics, ToolControlSemantics, ToolOutputTruncation, ToolRequest,
    ToolResult,
};
use orca_mcp::{McpElicitationHandler, McpElicitationRequest, McpElicitationResponse, McpRegistry};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

use crate::runtime_normal_tool::execute_runtime_normal_tool;
use crate::runtime_permission::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
    TurnPermissionOverlay, TurnPermissionOverlayDelta,
};
#[cfg(test)]
use crate::runtime_state::PermissionRuntimeState;
use crate::tasks::TaskRegistry;
use crate::terminal_service::TerminalService;

const READONLY_TOOL_TIMEOUT_SECS: u64 = 120;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const NORMAL_TOOL_MESSAGE_CAPACITY: usize = 32;
const NORMAL_TOOL_ADMISSION_PENDING: u8 = 0;
const NORMAL_TOOL_ADMISSION_STARTED: u8 = 1;
const NORMAL_TOOL_ADMISSION_CANCELLED: u8 = 2;

pub(crate) struct RuntimeReadonlyToolInvocation {
    pub(crate) request: ToolRequest,
    pub(crate) cwd: PathBuf,
    pub(crate) mcp_registry: McpRegistry,
    pub(crate) output_truncation: ToolOutputTruncation,
}

pub(crate) struct RuntimeNormalToolInvocation {
    pub(crate) request: ToolRequest,
    pub(crate) config: RunConfig,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_roots: Vec<PathBuf>,
    pub(crate) mcp_registry: McpRegistry,
    pub(crate) external_tools: Vec<ExternalToolConfig>,
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) shell_timeout_secs: u64,
    pub(crate) task_registry: Option<TaskRegistry>,
    pub(crate) terminal_service: Option<Arc<TerminalService>>,
    pub(crate) permission_overlay: TurnPermissionOverlay,
    pub(crate) control: ToolControlSemantics,
}

impl RuntimeNormalToolInvocation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn snapshot(
        config: &RunConfig,
        request: &ToolRequest,
        cwd: &std::path::Path,
        additional_roots: &[PathBuf],
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        permission_overlay: TurnPermissionOverlay,
    ) -> Self {
        let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
            Some(mcp_registry),
            external_tools,
        );
        let control = registry
            .control_semantics(&request.name)
            .unwrap_or(ToolControlSemantics {
                interrupt: InterruptSemantics::WaitForTerminal,
                replay: ReplaySemantics::IndeterminateAfterStart,
            });
        Self {
            request: request.clone(),
            config: config.clone(),
            cwd: cwd.to_path_buf(),
            additional_roots: additional_roots.to_vec(),
            mcp_registry: mcp_registry.clone(),
            external_tools: external_tools.to_vec(),
            output_truncation,
            shell_timeout_secs,
            task_registry: task_registry.cloned(),
            terminal_service: None,
            permission_overlay,
            control,
        }
    }

    pub(crate) fn with_terminal_service(
        mut self,
        terminal_service: Option<Arc<TerminalService>>,
    ) -> Self {
        self.terminal_service = terminal_service;
        self
    }
}

pub(crate) type RuntimeNormalToolOutputHandler<'a> = dyn FnMut(&str) -> io::Result<()> + 'a;

#[derive(Default)]
pub(crate) struct RuntimeNormalToolInteractions<'a> {
    pub(crate) output_handler: Option<&'a mut RuntimeNormalToolOutputHandler<'a>>,
    pub(crate) permission_handler: Option<&'a dyn RuntimePermissionRequestHandler>,
    pub(crate) mcp_elicitation_handler: Option<&'a dyn McpElicitationHandler>,
}

pub(crate) struct RuntimeNormalToolCallOutput {
    pub(crate) result: ToolResult,
    pub(crate) permission_delta: TurnPermissionOverlayDelta,
    pub(crate) event_error: Option<io::Error>,
}

pub(crate) struct RuntimeNormalToolWorkerContext<'a> {
    pub(crate) cancel: &'a CancelToken,
    pub(crate) permission_handler: Option<&'a dyn RuntimePermissionRequestHandler>,
    pub(crate) mcp_elicitation_handler: Option<&'a dyn McpElicitationHandler>,
    pub(crate) output_handler: Option<&'a mut dyn FnMut(&str)>,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
}

impl RuntimeNormalToolWorkerContext<'_> {
    #[cfg(test)]
    pub(crate) fn emit_output(&mut self, chunk: &str) {
        if let Some(handler) = self.output_handler.as_deref_mut() {
            handler(chunk);
        }
    }

    #[cfg(test)]
    pub(crate) fn request_permissions(
        &mut self,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let handler = self.permission_handler.ok_or_else(|| {
            io::Error::other("normal tool requested permissions without a runtime handler")
        })?;
        PermissionRuntimeState.request_permission(self.permission_overlay, handler, request)
    }
}

pub(crate) trait RuntimeNormalToolHandler: Send + Sync {
    fn execute(
        &self,
        invocation: &RuntimeNormalToolInvocation,
        context: &mut RuntimeNormalToolWorkerContext<'_>,
    ) -> ToolResult;
}

struct DefaultRuntimeNormalToolHandler;

impl RuntimeNormalToolHandler for DefaultRuntimeNormalToolHandler {
    fn execute(
        &self,
        invocation: &RuntimeNormalToolInvocation,
        context: &mut RuntimeNormalToolWorkerContext<'_>,
    ) -> ToolResult {
        execute_runtime_normal_tool(invocation, context)
    }
}

#[derive(Clone)]
struct RuntimeNormalToolWorkerBridge {
    sender: SyncSender<RuntimeNormalToolMessage>,
    cancel: CancelToken,
}

impl RuntimeNormalToolWorkerBridge {
    fn emit_output(&self, chunk: &str) {
        if self
            .sender
            .send(RuntimeNormalToolMessage::Output(chunk.to_string()))
            .is_err()
        {
            self.cancel.cancel();
        }
    }
}

impl RuntimePermissionRequestHandler for RuntimeNormalToolWorkerBridge {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.sender
            .send(RuntimeNormalToolMessage::Permission {
                request: request.clone(),
                response: response_sender,
            })
            .map_err(|_| io::Error::other("normal tool permission bridge closed"))?;
        response_receiver
            .recv()
            .map_err(|_| io::Error::other("normal tool permission response bridge closed"))?
    }
}

impl McpElicitationHandler for RuntimeNormalToolWorkerBridge {
    fn handle_elicitation(
        &self,
        request: McpElicitationRequest,
    ) -> Result<McpElicitationResponse, String> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        self.sender
            .send(RuntimeNormalToolMessage::McpElicitation {
                request,
                response: response_sender,
            })
            .map_err(|_| "normal tool MCP elicitation bridge closed".to_string())?;
        response_receiver
            .recv()
            .map_err(|_| "normal tool MCP elicitation response bridge closed".to_string())?
    }
}

enum RuntimeNormalToolMessage {
    Output(String),
    Permission {
        request: RuntimePermissionRequest,
        response: SyncSender<io::Result<RuntimePermissionResponse>>,
    },
    McpElicitation {
        request: McpElicitationRequest,
        response: SyncSender<Result<McpElicitationResponse, String>>,
    },
}

struct RuntimeNormalToolWorkerOutput {
    result: ToolResult,
    permission_overlay: TurnPermissionOverlay,
}

trait RuntimeReadonlyToolExecutor: Send + Sync {
    fn execute(
        &self,
        invocation: &RuntimeReadonlyToolInvocation,
        cancel: &CancelToken,
    ) -> ToolResult;
}

struct DefaultRuntimeReadonlyToolExecutor;

impl RuntimeReadonlyToolExecutor for DefaultRuntimeReadonlyToolExecutor {
    fn execute(
        &self,
        invocation: &RuntimeReadonlyToolInvocation,
        cancel: &CancelToken,
    ) -> ToolResult {
        orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
            &invocation.request,
            &invocation.cwd,
            &[],
            &invocation.mcp_registry,
            &[],
            invocation.output_truncation,
            READONLY_TOOL_TIMEOUT_SECS,
            None,
            || cancel.is_cancelled(),
        )
    }
}

struct RuntimeToolTerminal {
    result: Mutex<Option<ToolResult>>,
}

impl RuntimeToolTerminal {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
        }
    }

    fn complete(&self, result: ToolResult) -> bool {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(result);
        true
    }

    fn take(&self) -> Option<ToolResult> {
        self.result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

pub(crate) struct RuntimeToolCallRuntime {
    handle: Option<Handle>,
    executor: Arc<dyn RuntimeReadonlyToolExecutor>,
    normal_handler: Arc<dyn RuntimeNormalToolHandler>,
    _owned_runtime: Option<Arc<tokio::runtime::Runtime>>,
}

impl RuntimeToolCallRuntime {
    pub(crate) fn for_current_execution() -> io::Result<Self> {
        match Handle::try_current() {
            Ok(handle) => Ok(Self {
                handle: Some(handle),
                executor: Arc::new(DefaultRuntimeReadonlyToolExecutor),
                normal_handler: Arc::new(DefaultRuntimeNormalToolHandler),
                _owned_runtime: None,
            }),
            Err(_) => {
                let runtime = Arc::new(
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .thread_name("orca-tool-call")
                        .build()
                        .map_err(|error| {
                            io::Error::other(format!("failed to start tool-call runtime: {error}"))
                        })?,
                );
                Ok(Self {
                    handle: Some(runtime.handle().clone()),
                    executor: Arc::new(DefaultRuntimeReadonlyToolExecutor),
                    normal_handler: Arc::new(DefaultRuntimeNormalToolHandler),
                    _owned_runtime: Some(runtime),
                })
            }
        }
    }

    pub(crate) fn for_normal_execution() -> Self {
        Self {
            handle: None,
            executor: Arc::new(DefaultRuntimeReadonlyToolExecutor),
            normal_handler: Arc::new(DefaultRuntimeNormalToolHandler),
            _owned_runtime: None,
        }
    }

    #[cfg(test)]
    fn with_executor(executor: Arc<dyn RuntimeReadonlyToolExecutor>) -> io::Result<Self> {
        let mut runtime = Self::for_current_execution()?;
        runtime.executor = executor;
        Ok(runtime)
    }

    #[cfg(test)]
    pub(crate) fn with_normal_handler(
        handler: Arc<dyn RuntimeNormalToolHandler>,
    ) -> io::Result<Self> {
        let mut runtime = Self::for_normal_execution();
        runtime.normal_handler = handler;
        Ok(runtime)
    }

    pub(crate) fn execute_normal(
        &self,
        invocation: RuntimeNormalToolInvocation,
        parent_cancel: &CancelToken,
        mut interactions: RuntimeNormalToolInteractions<'_>,
    ) -> io::Result<RuntimeNormalToolCallOutput> {
        let request = invocation.request.clone();
        let baseline_overlay = invocation.permission_overlay.clone();
        let interrupt = invocation.control.interrupt;
        if interrupt == InterruptSemantics::DetachAndObserve {
            return Ok(RuntimeNormalToolCallOutput {
                result: ToolResult::failed_before_start(
                    &request,
                    "detach-and-observe requires a durable runtime observer owner",
                    None,
                ),
                permission_delta: TurnPermissionOverlayDelta::default(),
                event_error: None,
            });
        }
        if parent_cancel.is_cancelled() {
            return Ok(RuntimeNormalToolCallOutput {
                result: ToolResult::cancelled_before_start(
                    &request,
                    "the normal invocation was cancelled before dispatch",
                ),
                permission_delta: TurnPermissionOverlayDelta::default(),
                event_error: None,
            });
        }

        let admission = Arc::new(AtomicU8::new(NORMAL_TOOL_ADMISSION_PENDING));
        let child_cancel = CancelToken::new();
        let (message_sender, message_receiver) = mpsc::sync_channel(NORMAL_TOOL_MESSAGE_CAPACITY);
        let worker_bridge = RuntimeNormalToolWorkerBridge {
            sender: message_sender,
            cancel: child_cancel.clone(),
        };
        let handler = Arc::clone(&self.normal_handler);
        let worker_admission = Arc::clone(&admission);
        let worker_cancel = child_cancel.clone();
        let enable_output = interactions.output_handler.is_some();
        let enable_permissions = interactions.permission_handler.is_some();
        let enable_mcp_elicitation = interactions.mcp_elicitation_handler.is_some();
        let worker_request = request.clone();
        let join = match thread::Builder::new()
            .name("orca-normal-tool".to_string())
            .spawn(move || {
                let mut permission_overlay = invocation.permission_overlay.clone();
                if worker_admission
                    .compare_exchange(
                        NORMAL_TOOL_ADMISSION_PENDING,
                        NORMAL_TOOL_ADMISSION_STARTED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    return RuntimeNormalToolWorkerOutput {
                        result: ToolResult::cancelled_before_start(
                            &worker_request,
                            "the normal invocation was cancelled before dispatch",
                        ),
                        permission_overlay,
                    };
                }

                let execution = panic::catch_unwind(AssertUnwindSafe(|| {
                    let mut output_handler = |chunk: &str| worker_bridge.emit_output(chunk);
                    let mut context = RuntimeNormalToolWorkerContext {
                        cancel: &worker_cancel,
                        permission_handler: enable_permissions
                            .then_some(&worker_bridge as &dyn RuntimePermissionRequestHandler),
                        mcp_elicitation_handler: enable_mcp_elicitation
                            .then_some(&worker_bridge as &dyn McpElicitationHandler),
                        output_handler: enable_output
                            .then_some(&mut output_handler as &mut dyn FnMut(&str)),
                        permission_overlay: &mut permission_overlay,
                    };
                    handler.execute(&invocation, &mut context)
                }));
                let result = match execution {
                    Ok(result) => result,
                    Err(payload) => ToolResult::indeterminate_after_start(
                        &worker_request,
                        format!(
                            "Normal tool worker panicked after execution started: {}. Inspect external state before retrying.",
                            panic_payload_message(payload)
                        ),
                    ),
                };
                RuntimeNormalToolWorkerOutput {
                    result,
                    permission_overlay,
                }
            })
        {
            Ok(join) => join,
            Err(error) => {
                return Ok(RuntimeNormalToolCallOutput {
                    result: ToolResult::failed_before_start(
                        &request,
                        format!("failed to start normal tool worker: {error}"),
                        None,
                    ),
                    permission_delta: TurnPermissionOverlayDelta::default(),
                    event_error: None,
                });
            }
        };

        let mut event_error = None;
        let mut parent_cancellation_observed = false;
        loop {
            if !parent_cancellation_observed && parent_cancel.is_cancelled() {
                parent_cancellation_observed = true;
                signal_normal_tool_cancellation(&admission, interrupt, &child_cancel);
            }
            match message_receiver.recv_timeout(CANCELLATION_POLL_INTERVAL) {
                Ok(message) => handle_normal_tool_message(
                    message,
                    &mut interactions,
                    interrupt,
                    &child_cancel,
                    &mut event_error,
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if join.is_finished() {
                        break;
                    }
                }
            }
            if join.is_finished() {
                drain_normal_tool_messages(
                    &message_receiver,
                    &mut interactions,
                    interrupt,
                    &child_cancel,
                    &mut event_error,
                );
                break;
            }
        }

        let worker_output = match join.join() {
            Ok(output) => output,
            Err(payload) => RuntimeNormalToolWorkerOutput {
                result: if admission.load(Ordering::Acquire) == NORMAL_TOOL_ADMISSION_STARTED {
                    ToolResult::indeterminate_after_start(
                        &request,
                        format!(
                            "Normal tool task stopped after execution started: {}. Inspect external state before retrying.",
                            panic_payload_message(payload)
                        ),
                    )
                } else {
                    ToolResult::failed_before_start(
                        &request,
                        "normal tool task stopped before dispatch",
                        None,
                    )
                },
                permission_overlay: baseline_overlay.clone(),
            },
        };
        let terminal = RuntimeToolTerminal::new();
        let completed = terminal.complete(worker_output.result);
        debug_assert!(completed, "normal tool terminal must complete once");
        Ok(RuntimeNormalToolCallOutput {
            result: terminal.take().unwrap_or_else(|| {
                ToolResult::indeterminate(
                    &request,
                    "Normal tool task ended without a terminal result. Inspect external state before retrying.",
                )
            }),
            permission_delta: worker_output.permission_overlay.delta_from(&baseline_overlay),
            event_error,
        })
    }

    pub(crate) fn execute_readonly_batch(
        &self,
        invocations: Vec<RuntimeReadonlyToolInvocation>,
        max_parallel: usize,
        cancel: &CancelToken,
    ) -> Vec<ToolResult> {
        let executor = Arc::clone(&self.executor);
        let handle = self
            .handle
            .as_ref()
            .expect("read-only tool runtime handle")
            .clone();
        let cancel = cancel.clone();
        handle.block_on(async move {
            let permits = Arc::new(Semaphore::new(max_parallel.max(1)));
            let mut tasks = Vec::with_capacity(invocations.len());

            for invocation in invocations {
                let request = invocation.request.clone();
                let task_request = request.clone();
                let task_cancel = cancel.clone();
                let task_executor = Arc::clone(&executor);
                let task_permits = Arc::clone(&permits);
                let started = Arc::new(AtomicBool::new(false));
                let task_started = Arc::clone(&started);
                let terminal = Arc::new(RuntimeToolTerminal::new());
                let task_terminal = Arc::clone(&terminal);
                let join = tokio::spawn(async move {
                    let acquire = Arc::clone(&task_permits).acquire_owned();
                    tokio::pin!(acquire);
                    let permit = loop {
                        if task_cancel.is_cancelled() {
                            let completed = task_terminal.complete(
                                ToolResult::cancelled_before_start(
                                    &task_request,
                                    "the read-only invocation was cancelled before dispatch",
                                ),
                            );
                            debug_assert!(completed, "tool terminal must complete once");
                            return;
                        }
                        tokio::select! {
                            permit = &mut acquire => {
                                match permit {
                                    Ok(permit) => break permit,
                                    Err(_) => {
                                        let completed = task_terminal.complete(
                                            ToolResult::failed_before_start(
                                                &task_request,
                                                "read-only tool concurrency gate closed",
                                                None,
                                            ),
                                        );
                                        debug_assert!(completed, "tool terminal must complete once");
                                        return;
                                    }
                                }
                            }
                            _ = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {}
                        }
                    };

                    if task_cancel.is_cancelled() {
                        drop(permit);
                        let completed = task_terminal.complete(
                            ToolResult::cancelled_before_start(
                                &task_request,
                                "the read-only invocation was cancelled before dispatch",
                            ),
                        );
                        debug_assert!(completed, "tool terminal must complete once");
                        return;
                    }
                    task_started.store(true, Ordering::Release);
                    let blocking_cancel = task_cancel.clone();
                    let blocking = tokio::task::spawn_blocking(move || {
                        task_executor.execute(&invocation, &blocking_cancel)
                    })
                    .await;
                    drop(permit);

                    let result = match blocking {
                        Ok(result) => result,
                        Err(error) => ToolResult::indeterminate_after_start(
                            &task_request,
                            format!(
                                "Read-only tool worker panicked after execution started: {error}. Inspect external state before retrying."
                            ),
                        ),
                    };
                    let completed = task_terminal.complete(result);
                    debug_assert!(completed, "tool terminal must complete once");
                });
                tasks.push((request, started, terminal, join));
            }

            let mut results = Vec::with_capacity(tasks.len());
            for (request, started, terminal, join) in tasks {
                if let Err(error) = join.await {
                    let result = if started.load(Ordering::Acquire) {
                        ToolResult::indeterminate_after_start(
                            &request,
                            format!(
                                "Read-only tool task stopped after execution started: {error}. Inspect external state before retrying."
                            ),
                        )
                    } else {
                        ToolResult::failed_before_start(
                            &request,
                            format!("read-only tool task stopped before dispatch: {error}"),
                            None,
                        )
                    };
                    let _ = terminal.complete(result);
                }
                results.push(terminal.take().unwrap_or_else(|| {
                    ToolResult::indeterminate(
                        &request,
                        "Read-only tool task ended without a terminal result. Inspect external state before retrying.",
                    )
                }));
            }
            results
        })
    }
}

fn signal_normal_tool_cancellation(
    admission: &AtomicU8,
    interrupt: InterruptSemantics,
    child_cancel: &CancelToken,
) {
    if admission
        .compare_exchange(
            NORMAL_TOOL_ADMISSION_PENDING,
            NORMAL_TOOL_ADMISSION_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        child_cancel.cancel();
        return;
    }
    if admission.load(Ordering::Acquire) == NORMAL_TOOL_ADMISSION_STARTED
        && interrupt == InterruptSemantics::CooperativeCancel
    {
        child_cancel.cancel();
    }
}

fn handle_normal_tool_message(
    message: RuntimeNormalToolMessage,
    interactions: &mut RuntimeNormalToolInteractions<'_>,
    interrupt: InterruptSemantics,
    child_cancel: &CancelToken,
    event_error: &mut Option<io::Error>,
) {
    match message {
        RuntimeNormalToolMessage::Output(chunk) => {
            let Some(handler) = interactions.output_handler.as_deref_mut() else {
                return;
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| handler(&chunk))).unwrap_or_else(
                |payload| {
                    Err(io::Error::other(format!(
                        "normal tool output handler panicked: {}",
                        panic_payload_message(payload)
                    )))
                },
            );
            if let Err(error) = result {
                if event_error.is_none() {
                    *event_error = Some(error);
                }
                if interrupt == InterruptSemantics::CooperativeCancel {
                    child_cancel.cancel();
                }
            }
        }
        RuntimeNormalToolMessage::Permission { request, response } => {
            let result = match interactions.permission_handler {
                Some(handler) => {
                    panic::catch_unwind(AssertUnwindSafe(|| handler.request_permissions(&request)))
                        .unwrap_or_else(|payload| {
                            Err(io::Error::other(format!(
                                "normal tool permission handler panicked: {}",
                                panic_payload_message(payload)
                            )))
                        })
                }
                None => Err(io::Error::other(
                    "normal tool requested permissions without a parent runtime handler",
                )),
            };
            let _ = response.send(result);
        }
        RuntimeNormalToolMessage::McpElicitation { request, response } => {
            let result = match interactions.mcp_elicitation_handler {
                Some(handler) => {
                    panic::catch_unwind(AssertUnwindSafe(|| handler.handle_elicitation(request)))
                        .unwrap_or_else(|payload| {
                            Err(format!(
                                "normal tool MCP elicitation handler panicked: {}",
                                panic_payload_message(payload)
                            ))
                        })
                }
                None => Err(
                    "normal tool requested MCP elicitation without a parent runtime handler"
                        .to_string(),
                ),
            };
            let _ = response.send(result);
        }
    }
}

fn drain_normal_tool_messages(
    receiver: &Receiver<RuntimeNormalToolMessage>,
    interactions: &mut RuntimeNormalToolInteractions<'_>,
    interrupt: InterruptSemantics,
    child_cancel: &CancelToken,
    event_error: &mut Option<io::Error>,
) {
    loop {
        match receiver.try_recv() {
            Ok(message) => handle_normal_tool_message(
                message,
                interactions,
                interrupt,
                child_cancel,
                event_error,
            ),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Barrier, Condvar, mpsc};

    use orca_core::approval_types::ActionKind;
    use orca_core::config::RunConfig;
    use orca_core::config::file::FileConfig;
    use orca_core::tool_types::{
        InterruptSemantics, ReplaySemantics, ToolControlSemantics, ToolInvocationStarted, ToolName,
        ToolStatus, ToolTerminalSource,
    };

    use super::*;
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
        RequestPermissionProfile,
    };
    use crate::runtime_permission::{
        RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
        TurnPermissionOverlay,
    };
    use crate::tasks::TaskRegistry;

    struct CancelAwareExecutor {
        started: Arc<Barrier>,
    }

    impl RuntimeReadonlyToolExecutor for CancelAwareExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            cancel: &CancelToken,
        ) -> ToolResult {
            self.started.wait();
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::cancelled(&invocation.request, "cancelled in flight", None)
        }
    }

    struct OutOfOrderExecutor {
        both_started: Barrier,
        second_finished: (Mutex<bool>, Condvar),
        completion_order: Mutex<Vec<String>>,
    }

    impl RuntimeReadonlyToolExecutor for OutOfOrderExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            _cancel: &CancelToken,
        ) -> ToolResult {
            self.both_started.wait();
            if invocation.request.id == "first" {
                let (finished, wake) = &self.second_finished;
                let mut second_finished = finished
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                while !*second_finished {
                    second_finished = wake
                        .wait(second_finished)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                self.completion_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(invocation.request.id.clone());
            } else {
                self.completion_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(invocation.request.id.clone());
                let (finished, wake) = &self.second_finished;
                *finished
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
                wake.notify_all();
            }
            ToolResult::completed(&invocation.request, invocation.request.id.clone(), false)
        }
    }

    struct CompletionAfterCancellationExecutor {
        started: Arc<Barrier>,
    }

    impl RuntimeReadonlyToolExecutor for CompletionAfterCancellationExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            cancel: &CancelToken,
        ) -> ToolResult {
            self.started.wait();
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::completed(
                &invocation.request,
                "completed at cancellation".to_string(),
                false,
            )
        }
    }

    struct JoinTrackingExecutor {
        active: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
    }

    impl RuntimeReadonlyToolExecutor for JoinTrackingExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            _cancel: &CancelToken,
        ) -> ToolResult {
            self.active.fetch_add(1, Ordering::AcqRel);
            std::thread::sleep(Duration::from_millis(20));
            self.finished.fetch_add(1, Ordering::AcqRel);
            self.active.fetch_sub(1, Ordering::AcqRel);
            ToolResult::completed(&invocation.request, "joined".to_string(), false)
        }
    }

    struct AdmissionCancelExecutor {
        started: std::sync::mpsc::SyncSender<()>,
        calls: Arc<AtomicUsize>,
    }

    impl RuntimeReadonlyToolExecutor for AdmissionCancelExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            cancel: &CancelToken,
        ) -> ToolResult {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let _ = self.started.send(());
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::cancelled(&invocation.request, "cancelled in flight", None)
        }
    }

    struct PanickingExecutor;

    impl RuntimeReadonlyToolExecutor for PanickingExecutor {
        fn execute(
            &self,
            _invocation: &RuntimeReadonlyToolInvocation,
            _cancel: &CancelToken,
        ) -> ToolResult {
            panic!("read-only fixture panic");
        }
    }

    struct CooperativeNormalHandler {
        started: mpsc::SyncSender<()>,
        active: Arc<AtomicUsize>,
        cleaned: Arc<AtomicBool>,
    }

    impl RuntimeNormalToolHandler for CooperativeNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            self.active.fetch_add(1, Ordering::AcqRel);
            self.started.send(()).expect("report normal handler start");
            while !context.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(20));
            self.cleaned.store(true, Ordering::Release);
            self.active.fetch_sub(1, Ordering::AcqRel);
            ToolResult::cancelled(&invocation.request, "cancelled in flight", None)
        }
    }

    struct WaitForTerminalNormalHandler {
        started: mpsc::SyncSender<()>,
        release: Arc<AtomicBool>,
        observed_cancel: Arc<AtomicBool>,
    }

    impl RuntimeNormalToolHandler for WaitForTerminalNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            self.started.send(()).expect("report normal handler start");
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.observed_cancel
                .store(context.cancel.is_cancelled(), Ordering::Release);
            ToolResult::completed(
                &invocation.request,
                "observed completion".to_string(),
                false,
            )
        }
    }

    struct CompletionRaceNormalHandler {
        started: mpsc::SyncSender<()>,
    }

    impl RuntimeNormalToolHandler for CompletionRaceNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            self.started.send(()).expect("report normal handler start");
            while !context.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::completed(
                &invocation.request,
                "completed at cancellation".to_string(),
                false,
            )
        }
    }

    struct PanickingNormalHandler;

    impl RuntimeNormalToolHandler for PanickingNormalHandler {
        fn execute(
            &self,
            _invocation: &RuntimeNormalToolInvocation,
            _context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            panic!("normal handler fixture panic");
        }
    }

    struct OutputThenWaitNormalHandler {
        active: Arc<AtomicUsize>,
    }

    impl RuntimeNormalToolHandler for OutputThenWaitNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            self.active.fetch_add(1, Ordering::AcqRel);
            context.emit_output("streamed output");
            while !context.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.active.fetch_sub(1, Ordering::AcqRel);
            ToolResult::cancelled(&invocation.request, "output delivery failed", None)
        }
    }

    struct PermissionDeltaNormalHandler;

    impl RuntimeNormalToolHandler for PermissionDeltaNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            let request = RuntimePermissionRequest {
                id: invocation.request.id.clone(),
                reason: Some("write generated output".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![PathBuf::from("/granted")]),
                        entries: None,
                    }),
                    network: None,
                },
                context: crate::runtime_permission::RuntimePermissionContext::foreground(
                    crate::surface::SurfacePermissionOrigin::CommandExec,
                ),
            };
            context
                .request_permissions(request)
                .expect("typed permission bridge");
            ToolResult::completed(&invocation.request, "granted".to_string(), false)
        }
    }

    struct CountingNormalHandler {
        calls: Arc<AtomicUsize>,
    }

    impl RuntimeNormalToolHandler for CountingNormalHandler {
        fn execute(
            &self,
            invocation: &RuntimeNormalToolInvocation,
            _context: &mut RuntimeNormalToolWorkerContext<'_>,
        ) -> ToolResult {
            self.calls.fetch_add(1, Ordering::AcqRel);
            ToolResult::completed(&invocation.request, "called".to_string(), false)
        }
    }

    struct AllowPermissionHandler;

    impl RuntimePermissionRequestHandler for AllowPermissionHandler {
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

    fn invocation(id: &str) -> RuntimeReadonlyToolInvocation {
        RuntimeReadonlyToolInvocation {
            request: ToolRequest {
                id: id.to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some(id.to_string()),
                raw_arguments: None,
            },
            cwd: PathBuf::from("."),
            mcp_registry: McpRegistry::default(),
            output_truncation: ToolOutputTruncation::default(),
        }
    }

    fn normal_invocation(id: &str, interrupt: InterruptSemantics) -> RuntimeNormalToolInvocation {
        RuntimeNormalToolInvocation {
            request: ToolRequest {
                id: id.to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf test".to_string()),
                raw_arguments: None,
            },
            config: test_config(),
            cwd: PathBuf::from("."),
            additional_roots: Vec::new(),
            mcp_registry: McpRegistry::default(),
            external_tools: Vec::new(),
            output_truncation: ToolOutputTruncation::default(),
            shell_timeout_secs: 120,
            task_registry: Some(TaskRegistry::new(format!("normal-{id}"))),
            terminal_service: None,
            permission_overlay: TurnPermissionOverlay::default(),
            control: ToolControlSemantics {
                interrupt,
                replay: ReplaySemantics::IndeterminateAfterStart,
            },
        }
    }

    fn test_config() -> RunConfig {
        crate::command::config::assemble_run_config(
            crate::command::config::RunConfigRequest::new(
                "0.0.0-test",
                std::env::current_dir().expect("cwd"),
            ),
            FileConfig::default(),
        )
        .expect("test config")
    }

    #[test]
    fn in_flight_readonly_invocation_observes_cancellation_before_return() {
        let started = Arc::new(Barrier::new(2));
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(CancelAwareExecutor {
            started: Arc::clone(&started),
        }))
        .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_readonly_batch(vec![invocation("slow")], 1, &worker_cancel)
        });
        started.wait();
        cancel.cancel();

        let results = worker.join().expect("runtime worker");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ToolStatus::Cancelled);
        assert_eq!(results[0].terminal().started, ToolInvocationStarted::Yes);
    }

    #[test]
    fn readonly_results_preserve_provider_order_when_tasks_finish_out_of_order() {
        let executor = Arc::new(OutOfOrderExecutor {
            both_started: Barrier::new(2),
            second_finished: (Mutex::new(false), Condvar::new()),
            completion_order: Mutex::new(Vec::new()),
        });
        let runtime =
            RuntimeToolCallRuntime::with_executor(executor.clone()).expect("tool-call runtime");
        let results = runtime.execute_readonly_batch(
            vec![invocation("first"), invocation("second")],
            2,
            &CancelToken::new(),
        );

        assert_eq!(results[0].id, "first");
        assert_eq!(results[1].id, "second");
        assert_eq!(results[0].output.as_deref(), Some("first"));
        assert_eq!(results[1].output.as_deref(), Some("second"));
        assert_eq!(
            *executor
                .completion_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["second".to_string(), "first".to_string()]
        );
    }

    #[test]
    fn cancellation_before_permit_never_starts_waiting_invocation() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(AdmissionCancelExecutor {
            started: started_tx,
            calls: Arc::clone(&calls),
        }))
        .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_readonly_batch(
                vec![invocation("first"), invocation("second")],
                1,
                &worker_cancel,
            )
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("one invocation should acquire the permit");
        cancel.cancel();

        let results = worker.join().expect("runtime worker");
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result.terminal().started == ToolInvocationStarted::Yes)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.terminal().started == ToolInvocationStarted::No)
                .count(),
            1
        );
        assert!(
            results
                .iter()
                .all(|result| result.status == ToolStatus::Cancelled)
        );
    }

    #[test]
    fn observed_completion_wins_cancellation_race() {
        let started = Arc::new(Barrier::new(2));
        let runtime =
            RuntimeToolCallRuntime::with_executor(Arc::new(CompletionAfterCancellationExecutor {
                started: Arc::clone(&started),
            }))
            .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_readonly_batch(vec![invocation("race")], 1, &worker_cancel)
        });
        started.wait();
        cancel.cancel();

        let results = worker.join().expect("runtime worker");
        assert_eq!(results[0].status, ToolStatus::Completed);
        assert_eq!(
            results[0].output.as_deref(),
            Some("completed at cancellation")
        );
    }

    #[test]
    fn batch_returns_only_after_every_worker_is_joined() {
        let active = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(JoinTrackingExecutor {
            active: Arc::clone(&active),
            finished: Arc::clone(&finished),
        }))
        .expect("tool-call runtime");

        let results = runtime.execute_readonly_batch(
            vec![invocation("first"), invocation("second")],
            2,
            &CancelToken::new(),
        );

        assert_eq!(results.len(), 2);
        assert_eq!(finished.load(Ordering::Acquire), 2);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn readonly_worker_panic_is_indeterminate_after_start() {
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(PanickingExecutor))
            .expect("tool-call runtime");
        let results =
            runtime.execute_readonly_batch(vec![invocation("panic")], 1, &CancelToken::new());

        assert_eq!(results[0].status, ToolStatus::Indeterminate);
        assert_eq!(results[0].terminal().started, ToolInvocationStarted::Yes);
        assert_eq!(results[0].terminal().source, ToolTerminalSource::Observed);
    }

    #[test]
    fn runtime_tool_terminal_accepts_only_one_result() {
        let terminal = RuntimeToolTerminal::new();
        assert!(terminal.complete(ToolResult::completed(
            &invocation("once").request,
            "first".to_string(),
            false,
        )));
        assert!(!terminal.complete(ToolResult::failed(
            &invocation("once").request,
            "second",
            None,
        )));
        assert_eq!(terminal.take().unwrap().output.as_deref(), Some("first"));
    }

    #[test]
    fn normal_cancellation_before_admission_never_invokes_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(CountingNormalHandler {
                calls: Arc::clone(&calls),
            }))
            .expect("tool-call runtime");
        let cancel = CancelToken::new();
        cancel.cancel();

        let output = runtime
            .execute_normal(
                normal_invocation("cancel-before", InterruptSemantics::CooperativeCancel),
                &cancel,
                RuntimeNormalToolInteractions::default(),
            )
            .expect("normal tool output");

        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(output.result.status, ToolStatus::Cancelled);
        assert_eq!(output.result.terminal().started, ToolInvocationStarted::No);
    }

    #[test]
    fn cooperative_normal_cancellation_joins_cleanup_before_return() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicBool::new(false));
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(CooperativeNormalHandler {
                started: started_tx,
                active: Arc::clone(&active),
                cleaned: Arc::clone(&cleaned),
            }))
            .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_normal(
                normal_invocation("cooperative", InterruptSemantics::CooperativeCancel),
                &worker_cancel,
                RuntimeNormalToolInteractions::default(),
            )
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("normal handler start");
        cancel.cancel();

        let output = worker
            .join()
            .expect("normal runtime worker")
            .expect("normal tool output");
        assert_eq!(output.result.status, ToolStatus::Cancelled);
        assert!(cleaned.load(Ordering::Acquire));
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn wait_for_terminal_normal_tool_is_not_cancelled_after_start() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let release = Arc::new(AtomicBool::new(false));
        let observed_cancel = Arc::new(AtomicBool::new(false));
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(WaitForTerminalNormalHandler {
                started: started_tx,
                release: Arc::clone(&release),
                observed_cancel: Arc::clone(&observed_cancel),
            }))
            .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_normal(
                normal_invocation("wait", InterruptSemantics::WaitForTerminal),
                &worker_cancel,
                RuntimeNormalToolInteractions::default(),
            )
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("normal handler start");
        cancel.cancel();
        std::thread::sleep(Duration::from_millis(20));
        release.store(true, Ordering::Release);

        let output = worker
            .join()
            .expect("normal runtime worker")
            .expect("normal tool output");
        assert_eq!(output.result.status, ToolStatus::Completed);
        assert!(!observed_cancel.load(Ordering::Acquire));
    }

    #[test]
    fn normal_observed_completion_wins_cancellation_race() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(CompletionRaceNormalHandler {
                started: started_tx,
            }))
            .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_normal(
                normal_invocation("race", InterruptSemantics::CooperativeCancel),
                &worker_cancel,
                RuntimeNormalToolInteractions::default(),
            )
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("normal handler start");
        cancel.cancel();

        let output = worker
            .join()
            .expect("normal runtime worker")
            .expect("normal tool output");
        assert_eq!(output.result.status, ToolStatus::Completed);
        assert_eq!(
            output.result.output.as_deref(),
            Some("completed at cancellation")
        );
    }

    #[test]
    fn normal_worker_panic_is_indeterminate_after_start() {
        let runtime = RuntimeToolCallRuntime::with_normal_handler(Arc::new(PanickingNormalHandler))
            .expect("tool-call runtime");

        let output = runtime
            .execute_normal(
                normal_invocation("panic", InterruptSemantics::CooperativeCancel),
                &CancelToken::new(),
                RuntimeNormalToolInteractions::default(),
            )
            .expect("normal tool output");

        assert_eq!(output.result.status, ToolStatus::Indeterminate);
        assert_eq!(output.result.terminal().started, ToolInvocationStarted::Yes);
        assert_eq!(
            output.result.terminal().source,
            ToolTerminalSource::Observed
        );
    }

    #[test]
    fn normal_output_failure_cancels_and_joins_worker_before_return() {
        let active = Arc::new(AtomicUsize::new(0));
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(OutputThenWaitNormalHandler {
                active: Arc::clone(&active),
            }))
            .expect("tool-call runtime");
        let mut output_handler = |_chunk: &str| Err(io::Error::other("event sink closed"));

        let output = runtime
            .execute_normal(
                normal_invocation("output-failure", InterruptSemantics::CooperativeCancel),
                &CancelToken::new(),
                RuntimeNormalToolInteractions {
                    output_handler: Some(&mut output_handler),
                    ..RuntimeNormalToolInteractions::default()
                },
            )
            .expect("normal tool output");

        assert!(output.event_error.is_some());
        assert_eq!(output.result.status, ToolStatus::Cancelled);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn normal_parent_output_panic_still_cancels_and_joins_worker() {
        let active = Arc::new(AtomicUsize::new(0));
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(OutputThenWaitNormalHandler {
                active: Arc::clone(&active),
            }))
            .expect("tool-call runtime");
        let mut output_handler = |_chunk: &str| -> io::Result<()> {
            panic!("parent output fixture panic");
        };

        let output = runtime
            .execute_normal(
                normal_invocation("output-panic", InterruptSemantics::CooperativeCancel),
                &CancelToken::new(),
                RuntimeNormalToolInteractions {
                    output_handler: Some(&mut output_handler),
                    ..RuntimeNormalToolInteractions::default()
                },
            )
            .expect("normal tool output");

        assert!(
            output
                .event_error
                .as_ref()
                .is_some_and(|error| error.to_string().contains("output handler panicked"))
        );
        assert_eq!(output.result.status, ToolStatus::Cancelled);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn normal_permission_bridge_returns_typed_overlay_delta() {
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(PermissionDeltaNormalHandler))
                .expect("tool-call runtime");
        let permission_handler = AllowPermissionHandler;

        let output = runtime
            .execute_normal(
                normal_invocation("permission", InterruptSemantics::CooperativeCancel),
                &CancelToken::new(),
                RuntimeNormalToolInteractions {
                    permission_handler: Some(&permission_handler),
                    ..RuntimeNormalToolInteractions::default()
                },
            )
            .expect("normal tool output");

        assert_eq!(output.result.status, ToolStatus::Completed);
        assert_eq!(
            output.permission_delta.additional_working_directories(),
            &[PathBuf::from("/granted")]
        );
        let mut canonical_overlay = TurnPermissionOverlay::default();
        PermissionRuntimeState
            .merge_permission_delta(&mut canonical_overlay, &output.permission_delta);
        assert_eq!(
            canonical_overlay.additional_working_directories(),
            &[PathBuf::from("/granted")]
        );
    }

    #[test]
    fn detach_and_observe_normal_tool_is_rejected_before_start() {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime =
            RuntimeToolCallRuntime::with_normal_handler(Arc::new(CountingNormalHandler {
                calls: Arc::clone(&calls),
            }))
            .expect("tool-call runtime");

        let output = runtime
            .execute_normal(
                normal_invocation("detach", InterruptSemantics::DetachAndObserve),
                &CancelToken::new(),
                RuntimeNormalToolInteractions::default(),
            )
            .expect("normal tool output");

        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(output.result.status, ToolStatus::Failed);
        assert_eq!(output.result.terminal().started, ToolInvocationStarted::No);
    }
}
