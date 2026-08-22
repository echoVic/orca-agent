//! Integration tests for the ACP agent adapter layer.
//!
//! These tests drive `OrcaAcpAgent` directly (without the stdio transport) using
//! a scripted `ThreadOperationExecutor` to emit events that the ACP event
//! projector maps onto `SessionUpdate` notifications.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_client_protocol::{
    Agent, AgentSideConnection, AudioContent, CancelNotification, Client, ClientCapabilities,
    ClientSideConnection, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    FileSystemCapabilities, Implementation, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResourceLink, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextResourceContents,
};
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::provider_types::ProviderResponse;
use orca_core::subagent_config::SubagentConfig;
use orca_core::thread_identity::TurnId;
use orca_runtime::acp::OrcaAcpAgent;
use orca_runtime::model_response::RuntimeModelResponse;
use orca_runtime::runtime_host::{
    GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
    ThreadOperationOutcome,
};
use orca_runtime::surface::RuntimeSurfaceHostHandle;
use orca_runtime::thread::RuntimeThread;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct WireTestClient {
    updates: Arc<Mutex<Vec<SessionNotification>>>,
}

#[async_trait::async_trait(?Send)]
impl Client for WireTestClient {
    async fn request_permission(
        &self,
        _args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        self.updates.lock().unwrap().push(args);
        Ok(())
    }
}

fn wire_connection_pair(
    agent: OrcaAcpAgent,
    client: WireTestClient,
) -> (ClientSideConnection, AgentSideConnection, WireTestClient) {
    let (client_stream, agent_stream) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (agent_read, agent_write) = tokio::io::split(agent_stream);
    let (agent_connection, agent_io) = AgentSideConnection::new(
        agent,
        agent_write.compat_write(),
        agent_read.compat(),
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    let (client_connection, client_io) = ClientSideConnection::new(
        client.clone(),
        client_write.compat_write(),
        client_read.compat(),
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    tokio::task::spawn_local(agent_io);
    tokio::task::spawn_local(client_io);
    (client_connection, agent_connection, client)
}

#[test]
fn acp_wire_round_trip_projects_typed_prompt_updates() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::EmitMessageAndComplete {
            message: "wire hello".to_string(),
        },
    ]));
    let host = RuntimeHost::start_with_executor(executor).expect("start host");
    let (note_tx, mut note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );
    let client = WireTestClient::default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (client_connection, agent_connection, client_state) =
            wire_connection_pair(agent, client);
        tokio::task::spawn_local(async move {
            while let Some(notification) = note_rx.recv().await {
                if agent_connection
                    .session_notification(notification)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        client_connection
            .initialize(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("wire-test", "0.0.0")),
            )
            .await
            .expect("wire initialize");
        let session = client_connection
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("wire new session");
        let response = client_connection
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::from("wire prompt".to_string())],
            ))
            .await
            .expect("wire prompt");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        for _ in 0..50 {
            if !client_state.updates.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let updates = client_state.updates.lock().unwrap();
        assert!(updates.iter().any(|notification| {
            matches!(&notification.update, SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("wire hello")))
        }), "wire updates: {updates:?}");
    });
    host.shutdown().expect("shutdown host");
}

struct OrcaHomeGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
    _home: tempfile::TempDir,
}

impl OrcaHomeGuard {
    fn new() -> Self {
        let lock = ORCA_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().expect("create temporary ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        // This guard serializes environment mutation until the host shuts down.
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        Self {
            previous,
            _lock: lock,
            _home: home,
        }
    }
}

impl Drop for OrcaHomeGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(previous) => std::env::set_var("ORCA_HOME", previous),
                None => std::env::remove_var("ORCA_HOME"),
            }
        }
    }
}

fn test_config(cwd: PathBuf) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: orca_core::approval_types::ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).unwrap(),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}

// --- Scripted executor that emits events through the EventFactory ---

enum TestBehavior {
    EmitMessageAndComplete { message: String },
    Fail { message: String },
    WaitForCancel,
}

struct AcpTestExecutor {
    behaviors: Mutex<Vec<TestBehavior>>,
    calls: AtomicUsize,
    working_directories: Mutex<Vec<Option<PathBuf>>>,
    prompts: Mutex<Vec<String>>,
}

impl AcpTestExecutor {
    fn new(behaviors: Vec<TestBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors),
            calls: AtomicUsize::new(0),
            working_directories: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn working_directories(&self) -> Vec<Option<PathBuf>> {
        self.working_directories.lock().unwrap().clone()
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl ThreadOperationExecutor for AcpTestExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.working_directories
            .lock()
            .unwrap()
            .push(generation.config().cwd.clone());
        self.prompts
            .lock()
            .unwrap()
            .push(request.prompt().to_string());
        let behavior = self.behaviors.lock().unwrap().remove(0);
        match behavior {
            TestBehavior::EmitMessageAndComplete { message } => {
                let turn_request = request.thread_turn_request(generation);
                if let Some(ingress) = turn_request.provider_response_ingress() {
                    ingress.commit_response(&RuntimeModelResponse::new(
                        ProviderResponse {
                            steps: Vec::new(),
                            assistant_content: Some(message),
                            assistant_reasoning: None,
                            tool_calls: Vec::new(),
                            usage: None,
                        },
                        request.turn_id().clone(),
                    ))?;
                } else {
                    let identity = orca_core::thread_item_projection::ModelResponseIdentity::new(
                        TurnId::new(),
                    );
                    let mut sink = EventSink::new(writer, generation.config().output_format)
                        .with_optional_observer(request.event_observer());
                    sink.emit(events.assistant_message_delta(&identity, &message))?;
                }
                Ok(RunStatus::Success.into())
            }
            TestBehavior::Fail { message } => Err(io::Error::other(message)),
            TestBehavior::WaitForCancel => {
                let deadline = std::time::Instant::now() + TEST_TIMEOUT;
                while !cancel.is_cancelled() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "operation was not cancelled within timeout"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(RunStatus::Cancelled.into())
            }
        }
    }
}

// --- Helper to drain notifications from the channel ---

fn drain_notifications(rx: &mut mpsc::Receiver<SessionNotification>) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    while let Ok(notification) = rx.try_recv() {
        updates.push(notification.update);
    }
    updates
}

async fn initialize_agent(agent: &OrcaAcpAgent) {
    agent
        .initialize(InitializeRequest::new(ProtocolVersion::V1))
        .await
        .expect("initialize ACP connection");
}

// --- Tests ---

#[test]
fn acp_initialize_returns_exact_session_and_mcp_capabilities() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let response = local.block_on(&rt, async {
        agent
            .initialize(InitializeRequest::new(ProtocolVersion::V1))
            .await
            .expect("initialize")
    });

    assert_eq!(response.protocol_version, ProtocolVersion::V1);
    assert!(response.agent_capabilities.load_session);
    assert!(response.agent_capabilities.mcp_capabilities.sse);
    assert!(!response.agent_capabilities.mcp_capabilities.http);
    assert!(
        response
            .agent_capabilities
            .session_capabilities
            .additional_directories
            .is_some()
    );
    assert_eq!(
        response.agent_info.as_ref().map(|i| i.name.as_str()),
        Some("orca")
    );
    assert_eq!(
        response.agent_info.as_ref().map(|i| i.version.as_str()),
        Some("test")
    );

    host.shutdown().expect("shutdown");
}

#[test]
fn acp_session_commands_fail_closed_before_initialize() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let host =
        RuntimeHost::start_with_executor(Arc::new(AcpTestExecutor::new(vec![]))).expect("host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let errors = local.block_on(&rt, async {
        let new_error = agent
            .new_session(NewSessionRequest::new(cwd.path().to_path_buf()))
            .await
            .expect_err("new_session requires initialize");
        let load_error = agent
            .load_session(LoadSessionRequest::new(
                SessionId::new("not-loaded"),
                cwd.path().to_path_buf(),
            ))
            .await
            .expect_err("load_session requires initialize");
        let prompt_error = agent
            .prompt(PromptRequest::new(
                SessionId::new("not-loaded"),
                vec![ContentBlock::from("hello".to_string())],
            ))
            .await
            .expect_err("prompt requires initialize");
        [new_error, load_error, prompt_error]
    });

    for error in errors {
        assert!(format!("{error:?}").contains("not initialized"));
    }
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_new_session_persists_declared_additional_directories() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let response = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        agent
            .new_session(
                NewSessionRequest::new(cwd.path().to_path_buf())
                    .additional_directories(vec![extra.path().to_path_buf()]),
            )
            .await
            .expect("new session")
    });
    let transcript = RuntimeSurfaceHostHandle::load_saved_session(&response.session_id.to_string())
        .expect("saved ACP session");
    assert_eq!(transcript.meta.additional_working_directories.len(), 1);
    assert_eq!(
        transcript.meta.additional_working_directories[0].path,
        extra.path()
    );
    assert_eq!(
        transcript.meta.additional_working_directories[0].source,
        "acp"
    );
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_load_session_replaces_persisted_additional_directories() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let original = tempfile::tempdir().unwrap();
    let replacement = tempfile::tempdir().unwrap();

    let first_host =
        RuntimeHost::start_with_executor(Arc::new(AcpTestExecutor::new(vec![]))).expect("host");
    let (first_tx, _first_rx) = mpsc::channel::<SessionNotification>(256);
    let first_agent = OrcaAcpAgent::new(
        first_host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        first_tx,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let session = local.block_on(&rt, async {
        initialize_agent(&first_agent).await;
        first_agent
            .new_session(
                NewSessionRequest::new(cwd.path().to_path_buf())
                    .additional_directories(vec![original.path().to_path_buf()]),
            )
            .await
            .expect("new session")
    });
    first_host.shutdown().expect("shutdown first host");

    let second_host =
        RuntimeHost::start_with_executor(Arc::new(AcpTestExecutor::new(vec![]))).expect("host");
    let (second_tx, _second_rx) = mpsc::channel::<SessionNotification>(256);
    let second_agent = OrcaAcpAgent::new(
        second_host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        second_tx,
    );
    let wrong_cwd = tempfile::tempdir().unwrap();
    local.block_on(&rt, async {
        initialize_agent(&second_agent).await;
        assert!(
            second_agent
                .load_session(LoadSessionRequest::new(
                    session.session_id.clone(),
                    wrong_cwd.path().to_path_buf(),
                ))
                .await
                .is_err(),
            "load must reject a cwd that differs from the saved session"
        );
        second_agent
            .load_session(
                LoadSessionRequest::new(session.session_id.clone(), cwd.path().to_path_buf())
                    .additional_directories(vec![replacement.path().to_path_buf()]),
            )
            .await
            .expect("load session")
    });
    let transcript = RuntimeSurfaceHostHandle::load_saved_session(&session.session_id.to_string())
        .expect("loaded ACP session");
    assert_eq!(
        transcript.meta.additional_working_directories,
        vec![orca_core::config::AdditionalWorkingDirectory::new(
            replacement.path().to_path_buf(),
            "acp",
        )]
    );
    second_host.shutdown().expect("shutdown second host");
}

#[test]
fn acp_new_session_and_prompt_produces_message_chunk_notification() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::EmitMessageAndComplete {
            message: "Hello from Orca!".to_string(),
        },
    ]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, mut note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let (session_id, stop_reason) = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");

        let prompt_response = agent
            .prompt(PromptRequest::new(
                session.session_id.clone(),
                vec![ContentBlock::from("Say hello".to_string())],
            ))
            .await
            .expect("prompt");

        (session.session_id, prompt_response.stop_reason)
    });

    assert_eq!(stop_reason, StopReason::EndTurn);
    assert_eq!(executor.call_count(), 1);
    assert_eq!(
        executor.working_directories(),
        vec![Some(session_cwd.path().to_path_buf())]
    );

    let updates = drain_notifications(&mut note_rx);
    assert!(
        !updates.is_empty(),
        "should have received at least one session update"
    );
    let has_message_chunk = updates.iter().any(|update| {
        matches!(update, SessionUpdate::AgentMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("Hello from Orca!")))
    });
    assert!(
        has_message_chunk,
        "expected AgentMessageChunk with 'Hello from Orca!' in updates: {updates:?}"
    );

    drop(session_id);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_typed_prompt_preserves_supported_content_for_runtime_ingress() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::EmitMessageAndComplete {
            message: "content accepted".to_string(),
        },
    ]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let response = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![
                    ContentBlock::from("first".to_string()),
                    ContentBlock::Resource(EmbeddedResource::new(
                        EmbeddedResourceResource::TextResourceContents(
                            TextResourceContents::new("embedded", "file:///workspace/context.txt")
                                .mime_type("text/plain"),
                        ),
                    )),
                    ContentBlock::from("last".to_string()),
                ],
            ))
            .await
            .expect("supported content prompt")
    });

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(executor.call_count(), 1);
    assert_eq!(executor.prompts(), vec!["first\nembedded\nlast"]);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_resource_link_fails_closed_before_runtime_capability_route_exists() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let error = local.block_on(&rt, async {
        agent
            .initialize(
                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                    ClientCapabilities::new().fs(FileSystemCapabilities::new()
                        .read_text_file(true)
                        .write_text_file(false)),
                ),
            )
            .await
            .expect("initialize with read capability");
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::ResourceLink(ResourceLink::new(
                    "notes",
                    "file:///workspace/notes.txt",
                ))],
            ))
            .await
            .expect_err("resource link must fail closed without runtime capability route")
    });

    assert_eq!(executor.call_count(), 0);
    assert!(format!("{error:?}").contains("runtime-owned read capability route"));
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_resource_link_rejects_unadvertised_client_read_capability() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let error = local.block_on(&rt, async {
        agent
            .initialize(InitializeRequest::new(ProtocolVersion::V1))
            .await
            .expect("initialize without read capability");
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::ResourceLink(ResourceLink::new(
                    "notes",
                    "file:///workspace/notes.txt",
                ))],
            ))
            .await
            .expect_err("unadvertised client read capability must fail closed")
    });

    assert_eq!(executor.call_count(), 0);
    assert!(format!("{error:?}").contains("did not advertise fs/read_text_file"));
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_initialize_cannot_expand_negotiated_client_capabilities() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let (initialize_error, prompt_error) = local.block_on(&rt, async {
        agent
            .initialize(InitializeRequest::new(ProtocolVersion::V1))
            .await
            .expect("initial capability negotiation");
        let initialize_error = agent
            .initialize(
                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                    ClientCapabilities::new().fs(FileSystemCapabilities::new()
                        .read_text_file(true)
                        .write_text_file(true)),
                ),
            )
            .await
            .expect_err("second initialize must not expand client authority");
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        let prompt_error = agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::ResourceLink(ResourceLink::new(
                    "notes",
                    "file:///workspace/notes.txt",
                ))],
            ))
            .await
            .expect_err("failed renegotiation must not grant read capability");
        (initialize_error, prompt_error)
    });

    assert!(format!("{initialize_error:?}").contains("already initialized"));
    assert!(format!("{prompt_error:?}").contains("did not advertise fs/read_text_file"));
    assert_eq!(executor.call_count(), 0);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_typed_prompt_rejects_unsupported_content_before_reservation() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let error = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::Audio(AudioContent::new(
                    "base64-audio",
                    "audio/mpeg",
                ))],
            ))
            .await
            .expect_err("unsupported audio prompt must fail")
    });

    assert_eq!(executor.call_count(), 0);
    assert!(format!("{error:?}").contains("unsupported"));
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_typed_load_replays_surface_history_after_restart() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let first_executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::EmitMessageAndComplete {
            message: "history survives restart".to_string(),
        },
    ]));
    let first_host = RuntimeHost::start_with_executor(first_executor).expect("start first host");
    let (first_note_tx, _first_note_rx) = mpsc::channel::<SessionNotification>(256);
    let first_agent = OrcaAcpAgent::new(
        first_host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        first_note_tx,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let session_id = local.block_on(&rt, async {
        initialize_agent(&first_agent).await;
        let session = first_agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        first_agent
            .prompt(PromptRequest::new(
                session.session_id.clone(),
                vec![ContentBlock::from("persist this".to_string())],
            ))
            .await
            .expect("prompt");
        session.session_id
    });
    first_host.shutdown().expect("shutdown first host");

    let second_executor = Arc::new(AcpTestExecutor::new(vec![]));
    let second_host =
        RuntimeHost::start_with_executor(second_executor.clone()).expect("start second host");
    let (second_note_tx, mut second_note_rx) = mpsc::channel::<SessionNotification>(256);
    let second_agent = OrcaAcpAgent::new(
        second_host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        second_note_tx,
    );
    local.block_on(&rt, async {
        initialize_agent(&second_agent).await;
        second_agent
            .load_session(LoadSessionRequest::new(
                session_id,
                session_cwd.path().to_path_buf(),
            ))
            .await
            .expect("load_session");
    });

    let updates = drain_notifications(&mut second_note_rx);
    assert!(updates.iter().any(|update| {
        matches!(update, SessionUpdate::AgentMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("history survives restart")))
    }));
    assert!(updates.iter().any(|update| {
        matches!(update, SessionUpdate::UserMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("persist this")))
    }));
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::Plan(_)))
    );
    assert_eq!(second_executor.call_count(), 0);
    second_host.shutdown().expect("shutdown second host");
}

#[test]
fn acp_typed_surface_prompt_projects_runtime_batch_and_terminal() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::EmitMessageAndComplete {
            message: "Hello from typed surface!".to_string(),
        },
    ]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, mut note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let stop_reason = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("typed new_session");
        agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::from("Say hello".to_string())],
            ))
            .await
            .expect("typed prompt")
            .stop_reason
    });

    assert_eq!(stop_reason, StopReason::EndTurn);
    assert_eq!(executor.call_count(), 1);
    let updates = drain_notifications(&mut note_rx);
    assert!(updates.iter().any(|update| {
        matches!(update, SessionUpdate::AgentMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("Hello from typed surface!")))
    }));
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_typed_surface_prompt_releases_session_after_terminal_error() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::Fail {
            message: "typed failure".to_string(),
        },
        TestBehavior::EmitMessageAndComplete {
            message: "recovered typed prompt".to_string(),
        },
    ]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = agent
            .new_session(NewSessionRequest::new(cwd.path().to_path_buf()))
            .await
            .expect("typed new_session");
        assert!(
            agent
                .prompt(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::from("fail".to_string())],
                ))
                .await
                .is_err()
        );
        let recovered = agent
            .prompt(PromptRequest::new(
                session.session_id,
                vec![ContentBlock::from("recover".to_string())],
            ))
            .await
            .expect("typed prompt should be reusable after terminal error");
        assert_eq!(recovered.stop_reason, StopReason::EndTurn);
    });

    assert_eq!(executor.call_count(), 2);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_cancel_stops_in_flight_prompt() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![TestBehavior::WaitForCancel]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let stop_reason = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = Agent::new_session(&agent, NewSessionRequest::new(cwd.path().to_path_buf()))
            .await
            .expect("new_session");

        let session_id_for_prompt = session.session_id.clone();
        let session_id_for_cancel = session.session_id.clone();

        // Poll prompt first so it enters start_turn, then issue cancellation
        // immediately. This exercises the pre-install cancellation window.
        let prompt_fut = Agent::prompt(
            &agent,
            PromptRequest::new(
                session_id_for_prompt,
                vec![ContentBlock::from("long running".to_string())],
            ),
        );
        let cancel_fut = async {
            Agent::cancel(&agent, CancelNotification::new(session_id_for_cancel))
                .await
                .expect("cancel");
        };

        // Pin the prompt and run both concurrently.
        tokio::pin!(prompt_fut);
        tokio::pin!(cancel_fut);

        // Drive both: prompt completes once the compensation interrupt lands.
        let (prompt_result, _) = tokio::join!(prompt_fut, cancel_fut);
        prompt_result.expect("prompt").stop_reason
    });

    assert_eq!(stop_reason, StopReason::Cancelled);
    assert_eq!(executor.call_count(), 1);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_cancel_reports_runtime_commit_failure_before_retry_succeeds() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![TestBehavior::WaitForCancel]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        initialize_agent(&agent).await;
        let session = Agent::new_session(&agent, NewSessionRequest::new(cwd.path().to_path_buf()))
            .await
            .expect("new_session");
        let transcript =
            RuntimeSurfaceHostHandle::load_saved_session(&session.session_id.to_string())
                .expect("load active ACP transcript");
        let transcript_path = transcript.path;
        let backup_path = transcript_path.with_extension("cancel-error-backup");

        let prompt_fut = Agent::prompt(
            &agent,
            PromptRequest::new(
                session.session_id.clone(),
                vec![ContentBlock::from("long running".to_string())],
            ),
        );
        let cancel_fut = async {
            while executor.call_count() == 0 {
                tokio::task::yield_now().await;
            }
            std::fs::rename(&transcript_path, &backup_path).expect("hide ACP transcript");
            std::fs::create_dir(&transcript_path).expect("block ACP transcript writes");
            assert!(
                Agent::cancel(&agent, CancelNotification::new(session.session_id.clone()),)
                    .await
                    .is_err(),
                "ACP cancel must report a runtime commit failure"
            );
            std::fs::remove_dir(&transcript_path).expect("remove blocking transcript directory");
            std::fs::rename(&backup_path, &transcript_path).expect("restore ACP transcript");
            Agent::cancel(&agent, CancelNotification::new(session.session_id))
                .await
                .expect("retry ACP cancel after restoring persistence");
        };

        let (prompt_result, ()) = tokio::join!(prompt_fut, cancel_fut);
        assert_eq!(
            prompt_result.expect("cancelled prompt").stop_reason,
            StopReason::Cancelled
        );
    });

    assert_eq!(executor.call_count(), 1);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_prompt_on_unknown_session_returns_error() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor).expect("start host");
    let (note_tx, _note_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = OrcaAcpAgent::new(
        host.surface_handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let result = local.block_on(&rt, async {
        initialize_agent(&agent).await;
        agent
            .prompt(PromptRequest::new(
                SessionId::new("nonexistent-session"),
                vec![ContentBlock::from("hello".to_string())],
            ))
            .await
    });

    assert!(result.is_err(), "prompt on unknown session should fail");
    host.shutdown().expect("shutdown");
}
