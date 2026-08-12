//! ACP [`Agent`] implementation projected onto the runtime-owned typed surface.
//!
//! The adapter retains only ACP transport correlation. Runtime threads,
//! operation lifecycle, interactions, cancellation and terminal facts remain
//! owned by the runtime surface.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ClientCapabilities, ContentBlock, EmbeddedResourceResource, Error, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, McpServer, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest,
    PromptResponse, ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionAdditionalDirectoriesCapabilities,
    SessionCapabilities, SessionId, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use orca_core::config::{AdditionalWorkingDirectory, HistoryMode, RunConfig};
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc};

use crate::surface::{
    AcpRequestId, AssistantPatch, AttachResult, CanonicalMime, CanonicalPath, CanonicalUri,
    DisplayText, FreshAttachRequest, MutationReply, NonEmptyText, NonEmptyVec, NotAdmittedReason,
    OperationBudget, OperationIngressCorrelation, OperationKind, OperationRequestIntent,
    OperationSettingsPreparation, OperationTerminal, ReplayabilityRequest,
    RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, SequenceNumber,
    Sha256Digest, SurfaceAllowDeny, SurfaceAttachmentId, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceClientCommandError, SurfaceClientInteractionAnswer, SurfaceEvent, SurfaceInputRequest,
    SurfaceInputRequestBlock, SurfaceInteractionKind, SurfaceInteractionRequest,
    SurfaceInteractionRoute, SurfaceInteractionView, SurfaceOperationId, SurfaceRequestId,
    SurfaceSubscriptionItem, SurfaceToolResultKind, ToolPatch, TurnRequestBudgetScope,
    UncommittedMutation,
};

use crate::runtime_surface::{
    AcpAttachmentCapabilityProfile, AcpStandardCapabilitySet, CapabilityRevision, OperationPatch,
    RuntimeSurfaceRecordedThreadLoadError, SurfaceItem, SurfacePlanPriority, SurfacePlanStatus,
    SurfaceToolAction, SurfaceToolViewState,
};

pub(crate) const ACP_NOTIFICATION_CAPACITY: usize = 256;
const ACP_PERMISSION_REQUEST_CAPACITY: usize = 64;

#[derive(Clone)]
pub(crate) enum AcpNotificationSender {
    Buffered(mpsc::Sender<SessionNotification>),
    Acknowledged(mpsc::Sender<AcpNotificationDelivery>),
}

pub(crate) struct AcpNotificationDelivery {
    pub(crate) notification: SessionNotification,
    pub(crate) acknowledgement: std_mpsc::SyncSender<Result<(), String>>,
}

impl AcpNotificationSender {
    fn send(&self, notification: SessionNotification) -> Result<(), ()> {
        match self {
            Self::Buffered(sender) => sender.blocking_send(notification).map_err(|_| ()),
            Self::Acknowledged(sender) => {
                let (acknowledgement, receipt) = std_mpsc::sync_channel(1);
                sender
                    .blocking_send(AcpNotificationDelivery {
                        notification,
                        acknowledgement,
                    })
                    .map_err(|_| ())?;
                receipt.recv().map_err(|_| ())?.map_err(|_| ())
            }
        }
    }
}

/// Per-session runtime state held on the single-threaded ACP task.
struct SessionEntry {
    surface: RuntimeSurfaceHandle,
    prompt_binding: Option<AcpPromptBinding>,
    next_prompt_seq: u64,
}

enum AcpPromptBinding {
    Decoded {
        ready: Rc<Notify>,
    },
    Bound {
        ready: Rc<Notify>,
        client: RuntimeSurfaceClientHandle,
        inbound_seq: SequenceNumber,
    },
}

#[derive(Default)]
struct AgentState {
    sessions: HashMap<SessionId, SessionEntry>,
    client_capabilities: Option<AcpClientCapabilityProfile>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct AcpClientCapabilityProfile {
    revision: CapabilityRevision,
    read_text_file: bool,
    write_text_file: bool,
    terminal: bool,
}

impl AcpClientCapabilityProfile {
    #[cfg(test)]
    fn negotiated_for_test() -> Self {
        Self {
            revision: CapabilityRevision::try_new(1).expect("one is a valid capability revision"),
            read_text_file: false,
            write_text_file: false,
            terminal: false,
        }
    }

    fn attachment_profile(self) -> AcpAttachmentCapabilityProfile {
        AcpAttachmentCapabilityProfile {
            revision: self.revision,
            standard: AcpStandardCapabilitySet {
                file_read: self.read_text_file,
                file_write: self.write_text_file,
                terminal: self.terminal,
                ..AcpStandardCapabilitySet::default()
            },
        }
    }
}

impl From<&ClientCapabilities> for AcpClientCapabilityProfile {
    fn from(capabilities: &ClientCapabilities) -> Self {
        Self {
            revision: CapabilityRevision::try_new(1)
                .expect("initial ACP capability revision is valid"),
            read_text_file: capabilities.fs.read_text_file,
            write_text_file: capabilities.fs.write_text_file,
            terminal: capabilities.terminal,
        }
    }
}

/// ACP agent backed by the Orca runtime host.
pub struct OrcaAcpAgent {
    surface_host: RuntimeSurfaceHostHandle,
    base_config: RunConfig,
    note_tx: AcpNotificationSender,
    state: Rc<RefCell<AgentState>>,
    client_bridge: Option<Arc<AcpClientBridge>>,
}

pub(crate) struct AcpClientBridge {
    request_tx: mpsc::Sender<AcpPermissionRequest>,
    read_text_file_tx: Mutex<Option<mpsc::Sender<AcpReadTextFileRequest>>>,
    write_text_file_tx: Mutex<Option<mpsc::Sender<AcpWriteTextFileRequest>>>,
    terminal_create_tx: Mutex<Option<mpsc::Sender<AcpTerminalCreateRequest>>>,
    terminal_observation_tx: Mutex<Option<mpsc::Sender<AcpTerminalObservationRequest>>>,
    terminal_cleanup_tx: Mutex<Option<mpsc::Sender<AcpTerminalCleanupRequest>>>,
    state: Mutex<AcpClientBridgeState>,
    capability_write_notify: tokio::sync::Notify,
    next_key: AtomicU64,
}

struct AcpClientBridgeState {
    pending: HashMap<
        String,
        std_mpsc::SyncSender<Result<RequestPermissionResponse, AcpPermissionWaitError>>,
    >,
    cancelled_sessions: HashSet<String>,
    capability_writes: HashMap<String, usize>,
    capability_lanes_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcpPermissionWaitError {
    Cancelled,
    BridgeClosed,
    ResponseDropped,
    Client(String),
}

pub(crate) struct AcpPermissionRequest {
    pub request: RequestPermissionRequest,
    pub key: String,
}

pub(crate) struct AcpReadTextFileRequest {
    pub(crate) client: RuntimeSurfaceClientHandle,
    pub(crate) dispatch: crate::runtime_surface::AcpReadTextFileDispatch,
}

pub(crate) struct AcpWriteTextFileRequest {
    pub(crate) client: RuntimeSurfaceClientHandle,
    pub(crate) dispatch: crate::runtime_surface::AcpWriteTextFileDispatch,
}

pub(crate) struct AcpTerminalCreateRequest {
    pub(crate) client: RuntimeSurfaceClientHandle,
    pub(crate) dispatch: crate::runtime_surface::AcpTerminalCreateDispatch,
}

pub(crate) struct AcpTerminalObservationRequest {
    pub(crate) client: RuntimeSurfaceClientHandle,
    pub(crate) dispatch: crate::runtime_surface::AcpTerminalObservationDispatch,
}

pub(crate) struct AcpTerminalCleanupRequest {
    pub(crate) client: RuntimeSurfaceClientHandle,
    pub(crate) dispatch: crate::runtime_surface::AcpTerminalCleanupDispatch,
}

impl AcpClientBridge {
    #[cfg(test)]
    pub(crate) fn new() -> (Arc<Self>, mpsc::Receiver<AcpPermissionRequest>) {
        let (request_tx, request_rx) = mpsc::channel(ACP_PERMISSION_REQUEST_CAPACITY);
        (
            Arc::new(Self {
                request_tx,
                read_text_file_tx: Mutex::new(None),
                write_text_file_tx: Mutex::new(None),
                terminal_create_tx: Mutex::new(None),
                terminal_observation_tx: Mutex::new(None),
                terminal_cleanup_tx: Mutex::new(None),
                state: Mutex::new(AcpClientBridgeState {
                    pending: HashMap::new(),
                    cancelled_sessions: HashSet::new(),
                    capability_writes: HashMap::new(),
                    capability_lanes_closed: false,
                }),
                capability_write_notify: tokio::sync::Notify::new(),
                next_key: AtomicU64::new(1),
            }),
            request_rx,
        )
    }

    pub(crate) fn new_with_capability_lanes() -> (
        Arc<Self>,
        mpsc::Receiver<AcpPermissionRequest>,
        mpsc::Receiver<AcpReadTextFileRequest>,
        mpsc::Receiver<AcpWriteTextFileRequest>,
        mpsc::Receiver<AcpTerminalCreateRequest>,
        mpsc::Receiver<AcpTerminalObservationRequest>,
        mpsc::Receiver<AcpTerminalCleanupRequest>,
    ) {
        let (request_tx, request_rx) = mpsc::channel(ACP_PERMISSION_REQUEST_CAPACITY);
        let (read_text_file_tx, read_text_file_rx) = mpsc::channel(1);
        let (write_text_file_tx, write_text_file_rx) = mpsc::channel(1);
        let (terminal_create_tx, terminal_create_rx) = mpsc::channel(1);
        let (terminal_observation_tx, terminal_observation_rx) = mpsc::channel(1);
        let (terminal_cleanup_tx, terminal_cleanup_rx) = mpsc::channel(1);
        (
            Arc::new(Self {
                request_tx,
                read_text_file_tx: Mutex::new(Some(read_text_file_tx)),
                write_text_file_tx: Mutex::new(Some(write_text_file_tx)),
                terminal_create_tx: Mutex::new(Some(terminal_create_tx)),
                terminal_observation_tx: Mutex::new(Some(terminal_observation_tx)),
                terminal_cleanup_tx: Mutex::new(Some(terminal_cleanup_tx)),
                state: Mutex::new(AcpClientBridgeState {
                    pending: HashMap::new(),
                    cancelled_sessions: HashSet::new(),
                    capability_writes: HashMap::new(),
                    capability_lanes_closed: false,
                }),
                capability_write_notify: tokio::sync::Notify::new(),
                next_key: AtomicU64::new(1),
            }),
            request_rx,
            read_text_file_rx,
            write_text_file_rx,
            terminal_create_rx,
            terminal_observation_rx,
            terminal_cleanup_rx,
        )
    }

    fn dispatch_read_text_file(
        &self,
        client: RuntimeSurfaceClientHandle,
        dispatch: crate::runtime_surface::AcpReadTextFileDispatch,
    ) -> Result<(), crate::runtime_surface::AcpReadTextFileDispatch> {
        let Some(sender) = self
            .read_text_file_tx
            .lock()
            .expect("ACP read sender mutex is not poisoned")
            .as_ref()
            .cloned()
        else {
            return Err(dispatch);
        };
        sender
            .try_send(AcpReadTextFileRequest { client, dispatch })
            .map_err(|error| error.into_inner().dispatch)
    }

    fn dispatch_write_text_file(
        &self,
        client: RuntimeSurfaceClientHandle,
        dispatch: crate::runtime_surface::AcpWriteTextFileDispatch,
    ) -> Result<(), crate::runtime_surface::AcpWriteTextFileDispatch> {
        let Some(sender) = self
            .write_text_file_tx
            .lock()
            .expect("ACP write sender mutex is not poisoned")
            .as_ref()
            .cloned()
        else {
            return Err(dispatch);
        };
        sender
            .try_send(AcpWriteTextFileRequest { client, dispatch })
            .map_err(|error| error.into_inner().dispatch)
    }

    fn dispatch_terminal_create(
        &self,
        client: RuntimeSurfaceClientHandle,
        dispatch: crate::runtime_surface::AcpTerminalCreateDispatch,
    ) -> Result<(), crate::runtime_surface::AcpTerminalCreateDispatch> {
        let Some(sender) = self
            .terminal_create_tx
            .lock()
            .expect("ACP terminal create sender mutex is not poisoned")
            .as_ref()
            .cloned()
        else {
            return Err(dispatch);
        };
        sender
            .try_send(AcpTerminalCreateRequest { client, dispatch })
            .map_err(|error| error.into_inner().dispatch)
    }

    fn dispatch_terminal_cleanup(
        &self,
        client: RuntimeSurfaceClientHandle,
        dispatch: crate::runtime_surface::AcpTerminalCleanupDispatch,
    ) -> Result<(), crate::runtime_surface::AcpTerminalCleanupDispatch> {
        let Some(sender) = self
            .terminal_cleanup_tx
            .lock()
            .expect("ACP terminal cleanup sender mutex is not poisoned")
            .as_ref()
            .cloned()
        else {
            return Err(dispatch);
        };
        sender
            .try_send(AcpTerminalCleanupRequest { client, dispatch })
            .map_err(|error| error.into_inner().dispatch)
    }

    fn dispatch_terminal_observation(
        &self,
        client: RuntimeSurfaceClientHandle,
        dispatch: crate::runtime_surface::AcpTerminalObservationDispatch,
    ) -> Result<(), crate::runtime_surface::AcpTerminalObservationDispatch> {
        let Some(sender) = self
            .terminal_observation_tx
            .lock()
            .expect("ACP terminal observation sender mutex is not poisoned")
            .as_ref()
            .cloned()
        else {
            return Err(dispatch);
        };
        sender
            .try_send(AcpTerminalObservationRequest { client, dispatch })
            .map_err(|error| error.into_inner().dispatch)
    }

    pub(crate) fn begin_capability_write(&self, session_id: &SessionId) -> bool {
        let session_id = session_id.to_string();
        let mut state = self
            .state
            .lock()
            .expect("ACP client bridge mutex is not poisoned");
        if state.capability_lanes_closed || state.cancelled_sessions.contains(&session_id) {
            return false;
        }
        *state.capability_writes.entry(session_id).or_default() += 1;
        true
    }

    pub(crate) fn finish_capability_write(&self, session_id: &SessionId) {
        let session_id = session_id.to_string();
        let mut state = self
            .state
            .lock()
            .expect("ACP client bridge mutex is not poisoned");
        if let Some(count) = state.capability_writes.get_mut(&session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.capability_writes.remove(&session_id);
            }
        }
        drop(state);
        self.capability_write_notify.notify_waiters();
    }

    async fn wait_for_capability_writes(&self, session_id: &SessionId) {
        let session_id = session_id.to_string();
        loop {
            let notified = self.capability_write_notify.notified();
            if !self
                .state
                .lock()
                .expect("ACP client bridge mutex is not poisoned")
                .capability_writes
                .contains_key(&session_id)
            {
                return;
            }
            notified.await;
        }
    }

    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, AcpPermissionWaitError> {
        let key = format!(
            "{}\0{}",
            request.session_id,
            self.next_key.fetch_add(1, Ordering::Relaxed)
        );
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        let session_id = request.session_id.to_string();
        let mut state = self
            .state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned");
        if state.cancelled_sessions.contains(&session_id) {
            return Err(AcpPermissionWaitError::Cancelled);
        }
        state.pending.insert(key.clone(), reply_tx);
        drop(state);
        self.request_tx
            .try_send(AcpPermissionRequest {
                request,
                key: key.clone(),
            })
            .map_err(|_| {
                self.state
                    .lock()
                    .expect("ACP permission bridge mutex is not poisoned")
                    .pending
                    .remove(&key);
                AcpPermissionWaitError::BridgeClosed
            })?;
        let result = reply_rx
            .recv()
            .map_err(|_| AcpPermissionWaitError::ResponseDropped)?;
        self.state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .pending
            .remove(&key);
        result
    }

    pub(crate) fn begin_session(&self, session_id: &SessionId) {
        self.state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .cancelled_sessions
            .remove(&session_id.to_string());
    }

    pub(crate) fn cancel_session(&self, session_id: &SessionId) {
        let prefix = format!("{}\0", session_id);
        let pending = {
            let mut pending = self
                .state
                .lock()
                .expect("ACP permission bridge mutex is not poisoned");
            pending.cancelled_sessions.insert(session_id.to_string());
            let keys = pending
                .pending
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| pending.pending.remove(&key))
                .collect::<Vec<_>>()
        };
        for reply in pending {
            let _ = reply.send(Err(AcpPermissionWaitError::Cancelled));
        }
    }

    pub(crate) fn complete_permission(
        &self,
        key: &str,
        result: Result<RequestPermissionResponse, AcpPermissionWaitError>,
    ) {
        let reply = self
            .state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .pending
            .remove(key);
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    pub(crate) fn is_pending(&self, key: &str) -> bool {
        self.state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .pending
            .contains_key(key)
    }

    pub(crate) fn cancel_all(&self) {
        self.read_text_file_tx
            .lock()
            .expect("ACP read sender mutex is not poisoned")
            .take();
        self.write_text_file_tx
            .lock()
            .expect("ACP write sender mutex is not poisoned")
            .take();
        self.terminal_create_tx
            .lock()
            .expect("ACP terminal create sender mutex is not poisoned")
            .take();
        self.terminal_observation_tx
            .lock()
            .expect("ACP terminal observation sender mutex is not poisoned")
            .take();
        self.terminal_cleanup_tx
            .lock()
            .expect("ACP terminal cleanup sender mutex is not poisoned")
            .take();
        let pending = {
            let mut state = self
                .state
                .lock()
                .expect("ACP permission bridge mutex is not poisoned");
            state.capability_lanes_closed = true;
            state
                .pending
                .drain()
                .map(|(_, reply)| reply)
                .collect::<Vec<_>>()
        };
        for reply in pending {
            let _ = reply.send(Err(AcpPermissionWaitError::Cancelled));
        }
    }
}

impl OrcaAcpAgent {
    fn negotiated_client_capabilities(&self) -> Result<AcpClientCapabilityProfile, Error> {
        self.state
            .borrow()
            .client_capabilities
            .ok_or_else(|| Error::invalid_request().data("ACP connection is not initialized"))
    }

    pub fn new(
        host: RuntimeSurfaceHostHandle,
        base_config: RunConfig,
        note_tx: mpsc::Sender<SessionNotification>,
    ) -> Self {
        Self {
            surface_host: host.bind_new_connection(),
            base_config,
            note_tx: AcpNotificationSender::Buffered(note_tx),
            state: Rc::new(RefCell::new(AgentState::default())),
            client_bridge: None,
        }
    }

    pub(crate) fn new_supervised(
        host: RuntimeSurfaceHostHandle,
        base_config: RunConfig,
        note_tx: mpsc::Sender<AcpNotificationDelivery>,
    ) -> Self {
        Self {
            surface_host: host.bind_new_connection(),
            base_config,
            note_tx: AcpNotificationSender::Acknowledged(note_tx),
            state: Rc::new(RefCell::new(AgentState::default())),
            client_bridge: None,
        }
    }

    pub(crate) fn with_client_bridge(mut self, bridge: Arc<AcpClientBridge>) -> Self {
        self.client_bridge = Some(bridge);
        self
    }

    /// Builds a per-session config from the base config with the session cwd
    /// applied. Events flow through the observer, not the writer, so the
    /// output format is irrelevant.
    fn build_session_config(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        additional_directories: Vec<PathBuf>,
    ) -> Result<RunConfig, String> {
        let mut config = build_acp_session_config(
            self.base_config.clone(),
            cwd,
            mcp_servers,
            additional_directories,
        )?;
        config.prompt = String::new();
        config.show_session_picker = false;
        config.desktop_notifications = false;
        config.history_mode = HistoryMode::Record;
        Ok(config)
    }

    pub(crate) async fn admit_prompt(
        &self,
        args: PromptRequest,
        inbound_seq: Option<u64>,
    ) -> Result<AdmittedAcpPrompt, Error> {
        let client_capabilities = self.negotiated_client_capabilities()?;
        let ready = Rc::new(Notify::new());
        let (surface, inbound_seq) = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            if entry.prompt_binding.is_some() {
                return Err(Error::invalid_params().data("session already has an active prompt"));
            }
            let sequence = match inbound_seq {
                Some(sequence) => sequence,
                None => {
                    let sequence = entry.next_prompt_seq;
                    entry.next_prompt_seq =
                        entry.next_prompt_seq.checked_add(1).ok_or_else(|| {
                            Error::internal_error().data("ACP prompt sequence exhausted")
                        })?;
                    sequence
                }
            };
            entry.prompt_binding = Some(AcpPromptBinding::Decoded {
                ready: ready.clone(),
            });
            (entry.surface.clone(), sequence)
        };
        if let Some(bridge) = self.client_bridge.as_ref() {
            bridge.begin_session(&args.session_id);
        }
        let input = match decode_prompt_content(&args.prompt, client_capabilities) {
            Ok(input) => input,
            Err(message) => {
                self.clear_prompt_binding(&args.session_id, &ready);
                return Err(Error::invalid_params().data(message));
            }
        };
        let session_id = args.session_id.clone();
        let client_bridge = self.client_bridge.clone();
        let prepared = match tokio::task::spawn_blocking(move || {
            prepare_surface_prompt(
                &surface,
                &session_id,
                input,
                inbound_seq,
                client_capabilities,
                client_bridge,
            )
        })
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                self.clear_prompt_binding(&args.session_id, &ready);
                return Err(error.into_protocol_error());
            }
            Err(error) => {
                self.clear_prompt_binding(&args.session_id, &ready);
                return Err(Error::into_internal_error(error));
            }
        };

        {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            entry.prompt_binding = Some(AcpPromptBinding::Bound {
                ready: ready.clone(),
                client: prepared.client.clone(),
                inbound_seq: SequenceNumber::new(inbound_seq),
            });
        }
        ready.notify_waiters();
        Ok(AdmittedAcpPrompt {
            session_id: args.session_id,
            ready,
            prepared,
        })
    }

    pub(crate) async fn complete_prompt(
        &self,
        admitted: AdmittedAcpPrompt,
    ) -> Result<PromptResponse, Error> {
        let note_tx = self.note_tx.clone();
        let session_id = admitted.session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            drain_surface_prompt(admitted.prepared, session_id, note_tx)
        })
        .await;

        self.clear_prompt_binding(&admitted.session_id, &admitted.ready);
        let result = result
            .map_err(Error::into_internal_error)?
            .map_err(|message| Error::internal_error().data(message))?;
        Ok(PromptResponse::new(result))
    }

    fn clear_prompt_binding(&self, session_id: &SessionId, ready: &Rc<Notify>) {
        let mut state = self.state.borrow_mut();
        if let Some(entry) = state.sessions.get_mut(session_id) {
            let matches_prompt = match entry.prompt_binding.as_ref() {
                Some(AcpPromptBinding::Decoded { ready: current })
                | Some(AcpPromptBinding::Bound { ready: current, .. }) => {
                    Rc::ptr_eq(current, ready)
                }
                None => false,
            };
            if matches_prompt {
                entry.prompt_binding = None;
            }
        }
        ready.notify_waiters();
    }
}

fn build_acp_session_config(
    mut config: RunConfig,
    cwd: PathBuf,
    mcp_servers: Vec<McpServer>,
    additional_directories: Vec<PathBuf>,
) -> Result<RunConfig, String> {
    CanonicalPath::try_new(cwd.clone())
        .map_err(|error| format!("invalid ACP session cwd: {error:?}"))?;
    config.cwd = Some(cwd.clone());

    let mut roots = vec![cwd.clone()];
    let mut root_set = BTreeSet::from([cwd]);
    for root in config.runtime_workspace_roots.take().unwrap_or_default() {
        CanonicalPath::try_new(root.clone())
            .map_err(|error| format!("invalid ACP base workspace root: {error:?}"))?;
        if root_set.insert(root.clone()) {
            roots.push(root);
        }
    }
    for directory in &config.additional_working_directories {
        CanonicalPath::try_new(directory.path.clone())
            .map_err(|error| format!("invalid ACP base additional directory: {error:?}"))?;
        if root_set.insert(directory.path.clone()) {
            roots.push(directory.path.clone());
        }
    }
    for directory in additional_directories {
        CanonicalPath::try_new(directory.clone())
            .map_err(|error| format!("invalid ACP additional directory: {error:?}"))?;
        if !root_set.insert(directory.clone()) {
            return Err("duplicate ACP additional directory".to_string());
        }
        roots.push(directory.clone());
        config
            .additional_working_directories
            .push(AdditionalWorkingDirectory::new(directory, "acp"));
    }
    config.runtime_workspace_roots = Some(roots);

    let mut server_names = config
        .mcp_servers
        .iter()
        .map(|server| orca_mcp::canonical_server_name(&server.name))
        .collect::<HashSet<_>>();
    for server in mcp_servers {
        let mapped = map_acp_mcp_server(server)?;
        let canonical_name = orca_mcp::canonical_server_name(&mapped.name);
        if canonical_name.is_empty() || !server_names.insert(canonical_name) {
            return Err(format!("duplicate ACP MCP server '{}'", mapped.name));
        }
        config.mcp_servers.push(mapped);
    }
    Ok(config)
}

fn map_acp_mcp_server(server: McpServer) -> Result<McpServerConfig, String> {
    match server {
        McpServer::Stdio(server) => {
            validate_mcp_name(&server.name)?;
            CanonicalPath::try_new(server.command.clone()).map_err(|error| {
                format!(
                    "ACP MCP server '{}' has invalid absolute command: {error:?}",
                    server.name
                )
            })?;
            let command = server
                .command
                .to_str()
                .filter(|command| !command.is_empty())
                .ok_or_else(|| format!("ACP MCP server '{}' has invalid command", server.name))?
                .to_string();
            let mut env = HashMap::new();
            for variable in server.env {
                if variable.name.is_empty()
                    || env.insert(variable.name.clone(), variable.value).is_some()
                {
                    return Err(format!(
                        "ACP MCP server '{}' has invalid or duplicate environment name",
                        server.name
                    ));
                }
            }
            Ok(McpServerConfig {
                name: server.name,
                transport: McpTransportKind::Stdio,
                command: Some(command),
                args: server.args,
                url: None,
                env,
                headers: HashMap::new(),
                disabled: false,
                startup_timeout_ms: None,
                tool_timeout_ms: None,
            })
        }
        McpServer::Sse(server) => {
            validate_mcp_name(&server.name)?;
            CanonicalUri::try_new(server.url.clone()).map_err(|error| {
                format!(
                    "ACP MCP server '{}' has invalid SSE URL: {error:?}",
                    server.name
                )
            })?;
            if !matches!(
                server.url.split_once(':').map(|(scheme, _)| scheme),
                Some("http" | "https")
            ) {
                return Err(format!(
                    "ACP MCP server '{}' SSE URL must use http or https",
                    server.name
                ));
            }
            let mut headers = HashMap::new();
            for header in server.headers {
                let name = reqwest::header::HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|_| {
                        format!("ACP MCP server '{}' has invalid header name", server.name)
                    })?
                    .as_str()
                    .to_string();
                reqwest::header::HeaderValue::from_bytes(header.value.as_bytes()).map_err(
                    |_| format!("ACP MCP server '{}' has invalid header value", server.name),
                )?;
                if headers.insert(name, header.value).is_some() {
                    return Err(format!(
                        "ACP MCP server '{}' has duplicate header name",
                        server.name
                    ));
                }
            }
            Ok(McpServerConfig {
                name: server.name,
                transport: McpTransportKind::Sse,
                command: None,
                args: Vec::new(),
                url: Some(server.url),
                env: HashMap::new(),
                headers,
                disabled: false,
                startup_timeout_ms: None,
                tool_timeout_ms: None,
            })
        }
        McpServer::Http(server) => Err(format!(
            "HTTP MCP transport is not supported for ACP server '{}'",
            server.name
        )),
        _ => Err("unsupported ACP MCP transport".to_string()),
    }
}

fn validate_mcp_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        Err("ACP MCP server name cannot be empty".to_string())
    } else {
        Ok(())
    }
}

pub(crate) struct AdmittedAcpPrompt {
    session_id: SessionId,
    ready: Rc<Notify>,
    prepared: PreparedSurfacePrompt,
}

struct PreparedSurfacePrompt {
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: SurfaceOperationId,
    subscription: crate::surface::SurfaceSubscriptionReceiver,
    read_text_file_dispatch: Option<crate::runtime_surface::AcpReadTextFileDispatchReceiver>,
    write_text_file_dispatch: Option<crate::runtime_surface::AcpWriteTextFileDispatchReceiver>,
    terminal_create_dispatch: Option<crate::runtime_surface::AcpTerminalCreateDispatchReceiver>,
    terminal_observation_dispatch:
        Option<crate::runtime_surface::AcpTerminalObservationDispatchReceiver>,
    terminal_cleanup_dispatch: Option<crate::runtime_surface::AcpTerminalCleanupDispatchReceiver>,
    client_bridge: Option<Arc<AcpClientBridge>>,
    tool_outputs: HashMap<String, ToolOutputAccumulator>,
    detached: bool,
}

#[derive(Default)]
struct ToolOutputAccumulator {
    text: String,
    next_offset: u64,
}

impl PreparedSurfacePrompt {
    fn detach_once(&mut self) {
        if self.detached {
            return;
        }
        let _ = self.surface.detach(
            &self.client,
            crate::surface::DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        self.detached = true;
    }
}

impl Drop for PreparedSurfacePrompt {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        self.detach_once();
    }
}

struct SurfaceAttachmentGuard<'a> {
    surface: &'a RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: Option<SurfaceOperationId>,
    armed: bool,
}

impl<'a> SurfaceAttachmentGuard<'a> {
    fn new(surface: &'a RuntimeSurfaceHandle, client: RuntimeSurfaceClientHandle) -> Self {
        Self {
            surface,
            client,
            operation_id: None,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SurfaceAttachmentGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(operation_id) = self.operation_id.take() {
            let _ = self
                .client
                .cancel_operation(SurfaceRequestId::new(), operation_id);
        }
        let _ = self.surface.detach(
            &self.client,
            crate::surface::DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
    }
}

/// Decodes ACP prompt content into the closed runtime input algebra without
/// flattening away block identity or order.
fn decode_prompt_content(
    blocks: &[ContentBlock],
    client_capabilities: AcpClientCapabilityProfile,
) -> Result<SurfaceInputRequest, String> {
    let mut decoded = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                decoded.push(SurfaceInputRequestBlock::Text {
                    text: DisplayText::new(text.text.clone()),
                });
            }
            ContentBlock::ResourceLink(link) => {
                if !client_capabilities.read_text_file {
                    return Err(
                        "ACP client did not advertise fs/read_text_file for resource links"
                            .to_string(),
                    );
                }
                let uri = CanonicalUri::try_new(link.uri.clone())
                    .map_err(|error| format!("invalid ACP resource link URI: {error}"))?;
                let name = NonEmptyText::try_new(link.name.clone())
                    .map_err(|error| format!("invalid ACP resource link name: {error}"))?;
                let mime = link
                    .mime_type
                    .as_ref()
                    .map(|mime| {
                        CanonicalMime::try_new(mime.clone())
                            .map_err(|error| format!("invalid ACP resource link MIME: {error}"))
                    })
                    .transpose()?;
                decoded.push(SurfaceInputRequestBlock::ResourceLink {
                    uri,
                    name,
                    description: link.description.clone().map(DisplayText::new),
                    mime,
                });
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(resource) => {
                    let mime = resource.mime_type.as_deref().unwrap_or("text/plain");
                    if !mime.starts_with("text/") {
                        return Err(format!("unsupported ACP embedded text MIME: {mime}"));
                    }
                    let uri = CanonicalUri::try_new(resource.uri.clone())
                        .map_err(|error| format!("invalid ACP embedded resource URI: {error}"))?;
                    let mime = CanonicalMime::try_new(mime.to_string())
                        .map_err(|error| format!("invalid ACP embedded resource MIME: {error}"))?;
                    let digest = Sha256Digest::new(Sha256::digest(resource.text.as_bytes()).into());
                    decoded.push(SurfaceInputRequestBlock::EmbeddedText {
                        uri,
                        mime,
                        text: DisplayText::new(resource.text.clone()),
                        digest,
                    });
                }
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    return Err("unsupported ACP prompt content block: embedded_blob".to_string());
                }
                _ => {
                    return Err(
                        "unsupported ACP prompt content block: embedded_resource".to_string()
                    );
                }
            },
            _ => {
                return Err(format!(
                    "unsupported ACP prompt content block: {}",
                    content_block_name(block)
                ));
            }
        }
    }
    let blocks = NonEmptyVec::try_new(decoded)
        .map_err(|error| format!("invalid ACP prompt content: {error}"))?;
    Ok(SurfaceInputRequest { blocks })
}

fn content_block_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::ResourceLink(_) => "resource_link",
        ContentBlock::Resource(_) => "resource",
        _ => "unknown",
    }
}

fn replay_surface_snapshot(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    note_tx: &AcpNotificationSender,
) -> Result<(), String> {
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => return Err("ACP history attachment denied".to_string()),
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err("ACP history snapshot unavailable".to_string());
        }
    };
    let cleanup = SurfaceAttachmentGuard::new(surface, attachment.client.clone());
    for item in attachment.baseline.snapshot.items.iter() {
        let update = match item {
            SurfaceItem::UserMessage { input, .. } => replay_user_update(input),
            SurfaceItem::AssistantMessage { text, .. } => Some(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::ContentChunk::new(ContentBlock::from(
                    text.as_str().to_string(),
                )),
            )),
            SurfaceItem::AssistantReasoning { content, .. } => Some(
                SessionUpdate::AgentThoughtChunk(agent_client_protocol::ContentChunk::new(
                    ContentBlock::from(content.as_str().to_string()),
                )),
            ),
            SurfaceItem::AssistantPlan { .. } => None,
            SurfaceItem::ToolResultMessage { .. } => None,
            SurfaceItem::SystemMessage { .. } => None,
        };
        if let Some(update) = update {
            let _ = note_tx.send(SessionNotification::new(session_id.clone(), update));
        }
    }
    let known_tool_ids = attachment
        .baseline
        .snapshot
        .tools
        .iter()
        .map(|tool| tool.request.tool_call_id.clone())
        .collect::<HashSet<_>>();
    for tool in attachment.baseline.snapshot.tools.iter() {
        let call = ToolCall::new(
            ToolCallId::new(tool.request.tool_call_id.as_str().to_string()),
            tool_call_title(&tool.request),
        )
        .kind(tool_kind(tool.request.action))
        .status(match tool.state {
            SurfaceToolViewState::Requested => ToolCallStatus::Pending,
            SurfaceToolViewState::Running => ToolCallStatus::InProgress,
            SurfaceToolViewState::Completed => tool
                .result
                .as_ref()
                .map(|result| tool_status(result.terminal.kind))
                .unwrap_or(ToolCallStatus::Completed),
        })
        .raw_input(serde_json::from_str(tool.request.raw_arguments.as_str()).ok());
        let _ = note_tx.send(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCall(call),
        ));
        if let Some(result) = tool.result.as_ref() {
            let output = result
                .output
                .as_ref()
                .or(result.error.as_ref())
                .map(|value| value.as_str().to_string())
                .or_else(|| {
                    (!tool.streamed_output.as_str().is_empty())
                        .then(|| tool.streamed_output.as_str().to_string())
                });
            let mut fields = ToolCallUpdateFields::new().status(tool_status(result.terminal.kind));
            if let Some(output) = output {
                fields = fields.content(vec![ToolCallContent::from(ContentBlock::from(output))]);
            }
            let _ = note_tx.send(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(tool.request.tool_call_id.as_str().to_string()),
                    fields,
                )),
            ));
        }
    }
    for item in attachment.baseline.snapshot.items.iter() {
        let SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            terminal,
            ..
        } = item
        else {
            continue;
        };
        if known_tool_ids.contains(tool_call_id) {
            continue;
        }
        let _ = note_tx.send(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str().to_string()),
                ToolCallUpdateFields::new()
                    .status(tool_status(terminal.kind))
                    .content(vec![ToolCallContent::from(ContentBlock::from(
                        content.as_str().to_string(),
                    ))]),
            )),
        ));
    }
    let plan = Plan::new(
        attachment
            .baseline
            .snapshot
            .plan
            .items
            .iter()
            .map(|item| {
                PlanEntry::new(
                    item.step.as_str(),
                    match item.priority {
                        SurfacePlanPriority::Low => PlanEntryPriority::Low,
                        SurfacePlanPriority::Medium => PlanEntryPriority::Medium,
                        SurfacePlanPriority::High => PlanEntryPriority::High,
                    },
                    match item.status {
                        SurfacePlanStatus::Pending => PlanEntryStatus::Pending,
                        SurfacePlanStatus::InProgress => PlanEntryStatus::InProgress,
                        SurfacePlanStatus::Completed => PlanEntryStatus::Completed,
                    },
                )
            })
            .collect(),
    );
    let _ = note_tx.send(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::Plan(plan),
    ));
    cleanup.disarm();
    Ok(())
}

fn replay_user_update(
    input: &crate::runtime_surface::SurfaceUserInputState,
) -> Option<SessionUpdate> {
    let text = match input {
        crate::runtime_surface::SurfaceUserInputState::Pending { presentation, .. }
        | crate::runtime_surface::SurfaceUserInputState::ResolutionFailed {
            presentation, ..
        } => match presentation {
            crate::runtime_surface::SurfaceInputPresentation::Visible { text } => {
                Some(text.as_str().to_string())
            }
            crate::runtime_surface::SurfaceInputPresentation::Redacted => None,
        },
        crate::runtime_surface::SurfaceUserInputState::Resolved { fact } => match fact {
            crate::runtime_surface::SurfaceResolvedInputFact::Replayable { input, .. } => {
                Some(input.canonical_text.as_str().to_string())
            }
            crate::runtime_surface::SurfaceResolvedInputFact::NonReplayable {
                presentation,
                ..
            } => match presentation {
                crate::runtime_surface::SurfaceInputPresentation::Visible { text } => {
                    Some(text.as_str().to_string())
                }
                crate::runtime_surface::SurfaceInputPresentation::Redacted => None,
            },
        },
    }?;
    Some(SessionUpdate::UserMessageChunk(
        agent_client_protocol::ContentChunk::new(ContentBlock::from(text)),
    ))
}

fn tool_status(kind: SurfaceToolResultKind) -> ToolCallStatus {
    if kind == SurfaceToolResultKind::Success {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    }
}

#[derive(Debug)]
enum AcpPromptPrepareError {
    InvalidInput(String),
    Internal(String),
}

impl AcpPromptPrepareError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    fn into_protocol_error(self) -> Error {
        match self {
            Self::InvalidInput(message) => Error::invalid_params().data(message),
            Self::Internal(message) => Error::internal_error().data(message),
        }
    }
}

fn prepare_surface_prompt(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    input: SurfaceInputRequest,
    inbound_seq: u64,
    client_capabilities: AcpClientCapabilityProfile,
    client_bridge: Option<Arc<AcpClientBridge>>,
) -> Result<PreparedSurfacePrompt, AcpPromptPrepareError> {
    let attachment = match surface.attach_acp_fresh(
        FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Acp,
            requested_capabilities: std::collections::BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: standard_interaction_capabilities(),
        },
        client_capabilities.attachment_profile(),
    ) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface attachment denied",
            ));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface attachment unavailable",
            ));
        }
    };
    let mut cleanup = SurfaceAttachmentGuard::new(surface, attachment.client.clone());
    let subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| AcpPromptPrepareError::internal("ACP surface subscription unavailable"))?;
    let read_text_file_dispatch = if client_capabilities.read_text_file {
        Some(
            surface
                .claim_acp_read_text_file_dispatch(&attachment.client)
                .ok_or_else(|| {
                    AcpPromptPrepareError::internal(
                        "ACP read capability transport lane unavailable",
                    )
                })?,
        )
    } else {
        None
    };
    let write_text_file_dispatch = if client_capabilities.write_text_file {
        Some(
            surface
                .claim_acp_write_text_file_dispatch(&attachment.client)
                .ok_or_else(|| {
                    AcpPromptPrepareError::internal(
                        "ACP write capability transport lane unavailable",
                    )
                })?,
        )
    } else {
        None
    };
    let terminal_create_dispatch = if client_capabilities.terminal {
        Some(
            surface
                .claim_acp_terminal_create_dispatch(&attachment.client)
                .ok_or_else(|| {
                    AcpPromptPrepareError::internal(
                        "ACP terminal create transport lane unavailable",
                    )
                })?,
        )
    } else {
        None
    };
    let terminal_observation_dispatch = if client_capabilities.terminal {
        Some(
            surface
                .claim_acp_terminal_observation_dispatch(&attachment.client)
                .ok_or_else(|| {
                    AcpPromptPrepareError::internal(
                        "ACP terminal observation transport lane unavailable",
                    )
                })?,
        )
    } else {
        None
    };
    let terminal_cleanup_dispatch = if client_capabilities.terminal {
        Some(
            surface
                .claim_acp_terminal_cleanup_dispatch(&attachment.client)
                .ok_or_else(|| {
                    AcpPromptPrepareError::internal(
                        "ACP terminal cleanup transport lane unavailable",
                    )
                })?,
        )
    } else {
        None
    };
    let session_id = NonEmptyText::try_new(session_id.to_string()).map_err(|error| {
        AcpPromptPrepareError::invalid(format!("invalid ACP session id: {error}"))
    })?;
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::AcpPrompt {
            session_id,
            inbound_seq: SequenceNumber::new(inbound_seq),
            rpc_request_id: AcpRequestId::String(
                NonEmptyText::try_new(format!("prompt-{}", uuid::Uuid::new_v4())).map_err(
                    |error| {
                        AcpPromptPrepareError::invalid(format!("invalid ACP request id: {error}"))
                    },
                )?,
            ),
        },
        kind: OperationKind::UserTurn,
        input: Some(input),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: attachment.baseline.snapshot.settings.thread_revision,
            expected_policy_epoch: attachment.baseline.snapshot.settings.effective.policy_epoch,
        },
    };
    let reserved = match attachment
        .client
        .reserve_operation(SurfaceRequestId::new(), intent)
        .map_err(|error| {
            AcpPromptPrepareError::internal(format!("ACP surface reserve failed: {error:?}"))
        })? {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface reserve did not commit",
            ));
        }
        MutationReply::Uncommitted { mutation } => {
            let message = uncommitted_mutation_message(&mutation).to_string();
            return Err(match mutation {
                UncommittedMutation::Invalid { .. } | UncommittedMutation::Stale { .. } => {
                    AcpPromptPrepareError::invalid(message)
                }
                UncommittedMutation::Unavailable { .. }
                | UncommittedMutation::CommitFailed { .. } => {
                    AcpPromptPrepareError::internal(message)
                }
            });
        }
    };
    let operation_id = reserved.operation_id.clone();
    cleanup.operation_id = Some(operation_id.clone());
    match attachment
        .client
        .admit_reserved(
            SurfaceRequestId::new(),
            operation_id.clone(),
            reserved.lease.lease_id,
        )
        .map_err(|error| {
            AcpPromptPrepareError::internal(format!("ACP surface admission failed: {error:?}"))
        })? {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { .. } | MutationReply::Uncommitted { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface admission did not commit",
            ));
        }
    }
    cleanup.disarm();
    Ok(PreparedSurfacePrompt {
        surface: surface.clone(),
        client: attachment.client,
        operation_id,
        subscription,
        read_text_file_dispatch,
        write_text_file_dispatch,
        terminal_create_dispatch,
        terminal_observation_dispatch,
        terminal_cleanup_dispatch,
        client_bridge,
        tool_outputs: HashMap::new(),
        detached: false,
    })
}

fn standard_interaction_capabilities() -> std::collections::BTreeSet<SurfaceInteractionKind> {
    std::collections::BTreeSet::from([SurfaceInteractionKind::ToolApproval])
}

fn standard_acp_routes_interaction(
    attachment_id: &SurfaceAttachmentId,
    kind: SurfaceInteractionKind,
    route: &SurfaceInteractionRoute,
) -> bool {
    if kind != SurfaceInteractionKind::ToolApproval {
        return false;
    }
    match route {
        SurfaceInteractionRoute::Unassigned { .. } => false,
        SurfaceInteractionRoute::Exclusive {
            attachment_id: routed,
            ..
        } => routed == attachment_id,
        SurfaceInteractionRoute::SharedFirstCommitWins { attachments, .. } => {
            attachments.as_set().contains(attachment_id)
        }
    }
}

fn uncommitted_mutation_message(mutation: &UncommittedMutation) -> &str {
    match mutation {
        UncommittedMutation::Invalid { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Stale { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Unavailable { error, .. } => error.error().message.as_str(),
        UncommittedMutation::CommitFailed { error, .. } => error.error().message.as_str(),
    }
}

#[derive(Clone)]
enum AcpPermissionTarget {
    ToolApproval,
}

fn build_permission_request(
    session_id: &SessionId,
    interaction: &SurfaceInteractionView,
) -> Result<(RequestPermissionRequest, AcpPermissionTarget), String> {
    let (tool_call_id, title, target) = match &interaction.request {
        SurfaceInteractionRequest::ToolApproval {
            tool, description, ..
        } => (
            tool.tool_call_id.as_str().to_string(),
            description.as_str().to_string(),
            AcpPermissionTarget::ToolApproval,
        ),
        SurfaceInteractionRequest::PermissionRequest { .. }
        | SurfaceInteractionRequest::UserInput { .. }
        | SurfaceInteractionRequest::McpElicitation { .. }
        | SurfaceInteractionRequest::BackgroundApproval { .. } => {
            return Err("ACP client bridge does not support this interaction kind".to_string());
        }
    };
    let fields = ToolCallUpdateFields::new().title(title);
    let tool_call = ToolCallUpdate::new(ToolCallId::new(tool_call_id), fields);
    Ok((
        RequestPermissionRequest::new(
            session_id.clone(),
            tool_call,
            standard_tool_approval_options(),
        ),
        target,
    ))
}

fn standard_tool_approval_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "reject_once",
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
    ]
}

fn permission_answer(
    response: RequestPermissionResponse,
    target: AcpPermissionTarget,
) -> Result<SurfaceClientInteractionAnswer, String> {
    let allow = match response.outcome {
        RequestPermissionOutcome::Cancelled => false,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
            match option_id.to_string().as_str() {
                "allow_once" => true,
                "reject_once" => false,
                other => return Err(format!("unknown ACP permission option '{other}'")),
            }
        }
        _ => return Err("unsupported ACP permission outcome".to_string()),
    };
    Ok(match target {
        AcpPermissionTarget::ToolApproval => SurfaceClientInteractionAnswer::ToolApproval {
            decision: if allow {
                SurfaceAllowDeny::Allow
            } else {
                SurfaceAllowDeny::Deny
            },
        },
    })
}

fn drain_surface_prompt(
    mut prepared: PreparedSurfacePrompt,
    session_id: SessionId,
    note_tx: AcpNotificationSender,
) -> Result<StopReason, String> {
    let terminal = loop {
        drain_read_text_file_dispatch(&prepared);
        drain_write_text_file_dispatch(&prepared);
        drain_terminal_create_dispatch(&prepared);
        drain_terminal_observation_dispatch(&prepared);
        drain_terminal_cleanup_dispatch(&prepared);
        let Some(item) = prepared
            .subscription
            .recv_timeout(std::time::Duration::from_millis(100))
        else {
            continue;
        };
        match item {
            SurfaceSubscriptionItem::Batch { batch } => {
                let mut terminal = None;
                for envelope in batch.events.as_slice() {
                    project_surface_event(&mut prepared, &session_id, &note_tx, &envelope.event)?;
                    if let SurfaceEvent::Operation(OperationPatch::Terminal { record }) =
                        &envelope.event
                        && record.operation_id == prepared.operation_id
                    {
                        terminal = Some(record.terminal.clone());
                    }
                }
                if let Some(terminal) = terminal {
                    break terminal;
                }
            }
            SurfaceSubscriptionItem::Gap { .. } => {
                return reconcile_lost_subscription(&mut prepared, "gap");
            }
            SurfaceSubscriptionItem::Sealed { .. } => {
                return reconcile_lost_subscription(&mut prepared, "sealed");
            }
        }
    };
    prepared.detach_once();
    terminal_to_stop_reason(&terminal)
}

fn drain_write_text_file_dispatch(prepared: &PreparedSurfacePrompt) {
    let Some(receiver) = prepared.write_text_file_dispatch.as_ref() else {
        return;
    };
    loop {
        let dispatch = match receiver.try_recv() {
            Ok(dispatch) => dispatch,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Empty) => return,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Disconnected)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::StaleRoute)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::Full) => return,
        };
        let failed = match prepared.client_bridge.as_ref() {
            Some(bridge) => bridge.dispatch_write_text_file(prepared.client.clone(), dispatch),
            None => Err(dispatch),
        };
        if let Err(dispatch) = failed {
            let _ = prepared.client.settle_acp_write_text_file(
                dispatch.call_id,
                dispatch.capability_revision,
                crate::runtime_surface::AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write capability transport lane is unavailable".to_string(),
                },
            );
        }
    }
}

fn drain_terminal_create_dispatch(prepared: &PreparedSurfacePrompt) {
    let Some(receiver) = prepared.terminal_create_dispatch.as_ref() else {
        return;
    };
    loop {
        let dispatch = match receiver.try_recv() {
            Ok(dispatch) => dispatch,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Empty) => return,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Disconnected)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::StaleRoute)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::Full) => return,
        };
        let failed = match prepared.client_bridge.as_ref() {
            Some(bridge) => bridge.dispatch_terminal_create(prepared.client.clone(), dispatch),
            None => Err(dispatch),
        };
        if let Err(dispatch) = failed {
            let _ = prepared.client.settle_acp_terminal_create(
                dispatch.call_id,
                dispatch.capability_revision,
                crate::runtime_surface::AcpTerminalCreateSettlement::FailedBeforeWrite {
                    message: "ACP terminal create transport lane is unavailable".to_string(),
                },
            );
        }
    }
}

fn drain_terminal_cleanup_dispatch(prepared: &PreparedSurfacePrompt) {
    let Some(receiver) = prepared.terminal_cleanup_dispatch.as_ref() else {
        return;
    };
    loop {
        let dispatch = match receiver.try_recv() {
            Ok(dispatch) => dispatch,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Empty) => return,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Disconnected)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::StaleRoute)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::Full) => return,
        };
        let failed = match prepared.client_bridge.as_ref() {
            Some(bridge) => bridge.dispatch_terminal_cleanup(prepared.client.clone(), dispatch),
            None => Err(dispatch),
        };
        if let Err(dispatch) = failed {
            let _ = prepared.client.settle_acp_terminal_cleanup(
                dispatch.call_id,
                dispatch.capability_revision,
                crate::runtime_surface::AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal cleanup transport lane is unavailable".to_string(),
                },
            );
        }
    }
}

fn drain_terminal_observation_dispatch(prepared: &PreparedSurfacePrompt) {
    let Some(receiver) = prepared.terminal_observation_dispatch.as_ref() else {
        return;
    };
    loop {
        let dispatch = match receiver.try_recv() {
            Ok(dispatch) => dispatch,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Empty) => return,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Disconnected)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::StaleRoute)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::Full) => return,
        };
        let failed = match prepared.client_bridge.as_ref() {
            Some(bridge) => bridge.dispatch_terminal_observation(prepared.client.clone(), dispatch),
            None => Err(dispatch),
        };
        if let Err(dispatch) = failed {
            let _ = prepared.client.settle_acp_terminal_observation(
                dispatch.call_id,
                dispatch.capability_revision,
                crate::runtime_surface::AcpTerminalObservationSettlement::FailedBeforeWrite {
                    message: "ACP terminal observation transport lane is unavailable".to_string(),
                },
            );
        }
    }
}

fn drain_read_text_file_dispatch(prepared: &PreparedSurfacePrompt) {
    let Some(receiver) = prepared.read_text_file_dispatch.as_ref() else {
        return;
    };
    loop {
        let dispatch = match receiver.try_recv() {
            Ok(dispatch) => dispatch,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Empty) => return,
            Err(crate::runtime_surface::AcpCapabilityDispatchError::Disconnected)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::StaleRoute)
            | Err(crate::runtime_surface::AcpCapabilityDispatchError::Full) => return,
        };
        let failed = match prepared.client_bridge.as_ref() {
            Some(bridge) => bridge.dispatch_read_text_file(prepared.client.clone(), dispatch),
            None => Err(dispatch),
        };
        if let Err(dispatch) = failed {
            let _ = prepared.client.settle_acp_read_text_file(
                dispatch.call_id,
                dispatch.capability_revision,
                crate::runtime_surface::AcpReadTextFileSettlement::FailedBeforeWrite {
                    message: "ACP read capability transport lane is unavailable".to_string(),
                },
            );
        }
    }
}

fn reconcile_lost_subscription(
    prepared: &mut PreparedSurfacePrompt,
    loss: &str,
) -> Result<StopReason, String> {
    let sealed_snapshot = prepared.subscription.sealed_snapshot();
    prepared.detach_once();
    if let Some(snapshot) = sealed_snapshot {
        return reconcile_operation_snapshot(prepared, loss, snapshot.snapshot.as_ref());
    }
    let attachment = match prepared.surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. }
        | AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            if let Some(snapshot) = prepared.subscription.sealed_snapshot() {
                return reconcile_operation_snapshot(prepared, loss, snapshot.snapshot.as_ref());
            }
            return Err(format!(
                "ACP surface subscription {loss}; durable snapshot reconciliation unavailable"
            ));
        }
    };
    let cleanup = SurfaceAttachmentGuard::new(&prepared.surface, attachment.client.clone());
    let result =
        reconcile_operation_snapshot(prepared, loss, attachment.baseline.snapshot.as_ref());
    cleanup.disarm();
    let _ = prepared.surface.detach(
        &attachment.client,
        crate::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
    result
}

fn reconcile_operation_snapshot(
    prepared: &PreparedSurfacePrompt,
    loss: &str,
    snapshot: &crate::runtime_surface::SurfaceSnapshot,
) -> Result<StopReason, String> {
    let terminal = snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .find(|operation| operation.operation_id == prepared.operation_id)
        .and_then(|operation| operation.terminal.as_ref())
        .map(|record| record.terminal.clone());
    match terminal {
        Some(terminal) => {
            let reason = terminal_to_stop_reason(&terminal)
                .map(|reason| format!("{reason:?}"))
                .unwrap_or_else(|error| error);
            Err(format!(
                "ACP surface subscription {loss} after durable terminal {reason}; reload session"
            ))
        }
        None => Err(format!(
            "ACP surface subscription {loss} before terminal; reload session"
        )),
    }
}

fn terminal_to_stop_reason(terminal: &OperationTerminal) -> Result<StopReason, String> {
    match terminal {
        OperationTerminal::Succeeded { .. } => Ok(StopReason::EndTurn),
        OperationTerminal::Cancelled { .. } => Ok(StopReason::Cancelled),
        OperationTerminal::BudgetExhausted {
            budget: OperationBudget::ModelTokens { .. },
        } => Ok(StopReason::MaxTokens),
        OperationTerminal::BudgetExhausted {
            budget:
                OperationBudget::TurnRequests {
                    scope: TurnRequestBudgetScope::AgentLoop,
                    ..
                },
        } => Ok(StopReason::MaxTurnRequests),
        OperationTerminal::BudgetExhausted { budget } => {
            // The private stable error name for budget dimensions ACP cannot
            // represent as a standard StopReason (Subagent/Goal/Workflow/
            // monetary/tool-call/wall-time budgets); the exact terminal
            // metadata rides on the Orca surface record.
            Err(format!("OrcaBudgetExhausted: {budget:?}"))
        }
        OperationTerminal::NotAdmitted {
            reason: NotAdmittedReason::CancelledBeforeAdmission,
        } => Ok(StopReason::Cancelled),
        OperationTerminal::NotAdmitted { reason } => {
            Err(format!("ACP operation was not admitted: {reason:?}"))
        }
        OperationTerminal::Failed { message, .. }
        | OperationTerminal::Panicked { message }
        | OperationTerminal::JoinFailed { message } => Err(message.as_str().to_string()),
        OperationTerminal::AbortedByRuntimeRestart { .. } => {
            Err("ACP operation aborted by runtime restart".to_string())
        }
        OperationTerminal::Shutdown { .. } => Ok(StopReason::Cancelled),
    }
}

fn emit_surface_event(
    session_id: &SessionId,
    note_tx: &AcpNotificationSender,
    event: &SurfaceEvent,
    tool_outputs: &mut HashMap<String, ToolOutputAccumulator>,
) {
    let update = match event {
        SurfaceEvent::Assistant(AssistantPatch::Delta { text, .. }) => Some(
            SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                ContentBlock::from(text.as_str().to_string()),
            )),
        ),
        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response }) => {
            response.message_item.as_ref().map(|item| {
                SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                    ContentBlock::from(item.text.as_str().to_string()),
                ))
            })
        }
        SurfaceEvent::Tool(ToolPatch::Requested { request }) => Some(SessionUpdate::ToolCall(
            ToolCall::new(
                ToolCallId::new(request.tool_call_id.as_str().to_string()),
                tool_call_title(request),
            )
            .kind(tool_kind(request.action))
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::from_str(request.raw_arguments.as_str()).ok()),
        )),
        SurfaceEvent::Tool(ToolPatch::OutputDelta {
            tool_call_id,
            offset,
            chunk,
        }) => {
            let output = tool_outputs
                .entry(tool_call_id.as_str().to_string())
                .or_default();
            let start = offset.get();
            if start > output.next_offset {
                return;
            }
            let overlap = output.next_offset.saturating_sub(start) as usize;
            if overlap >= chunk.as_str().len() || !chunk.as_str().is_char_boundary(overlap) {
                return;
            }
            output.text.push_str(&chunk.as_str()[overlap..]);
            output.next_offset = start + chunk.as_str().len() as u64;
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str().to_string()),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .content(vec![ToolCallContent::from(ContentBlock::from(
                        output.text.clone(),
                    ))]),
            )))
        }
        SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
            let output = result
                .output
                .as_ref()
                .or(result.error.as_ref())
                .map(|text| text.as_str().to_string());
            let accumulated = tool_outputs
                .entry(result.tool_call_id.as_str().to_string())
                .or_default();
            if let Some(output) = output {
                accumulated.next_offset = output.len() as u64;
                accumulated.text = output;
            }
            let status = if result.terminal.kind == SurfaceToolResultKind::Success {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            };
            let mut fields = ToolCallUpdateFields::new().status(status);
            if !accumulated.text.is_empty() {
                fields = fields.content(vec![ToolCallContent::from(ContentBlock::from(
                    accumulated.text.clone(),
                ))]);
            }
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(result.tool_call_id.as_str().to_string()),
                fields,
            )))
        }
        _ => None,
    };
    if let Some(update) = update {
        let _ = note_tx.send(SessionNotification::new(session_id.clone(), update));
    }
}

fn project_surface_event(
    prepared: &mut PreparedSurfacePrompt,
    session_id: &SessionId,
    note_tx: &AcpNotificationSender,
    event: &SurfaceEvent,
) -> Result<(), String> {
    if let SurfaceEvent::Interaction(crate::surface::InteractionPatch::Requested { interaction }) =
        event
        && standard_acp_routes_interaction(
            prepared.client.attachment_id(),
            interaction.kind,
            &interaction.route,
        )
    {
        let Some(bridge) = prepared.client_bridge.as_ref() else {
            cancel_surface_operation(prepared)?;
            return Err("ACP interaction requires a connected client bridge".to_string());
        };
        let (request, target) = match build_permission_request(session_id, interaction) {
            Ok(value) => value,
            Err(error) => {
                cancel_surface_operation(prepared)?;
                return Err(error);
            }
        };
        let response = match bridge.request_permission(request) {
            Ok(response) => response,
            Err(AcpPermissionWaitError::Cancelled) => {
                let _ = cancel_surface_operation(prepared);
                return Ok(());
            }
            Err(error) => {
                cancel_surface_operation(prepared)?;
                return Err(format!("ACP permission request failed: {error:?}"));
            }
        };
        let answer = match permission_answer(response, target) {
            Ok(answer) => answer,
            Err(error) => {
                let _ = cancel_surface_operation(prepared);
                return Err(error);
            }
        };
        match prepared.client.respond_interaction_by_id(
            SurfaceRequestId::new(),
            interaction.interaction_id.clone(),
            answer,
        ) {
            Ok(MutationReply::Committed { .. }) => {}
            Ok(MutationReply::Deferred { .. }) => {
                let _ = cancel_surface_operation(prepared);
                return Err("ACP interaction response was deferred".to_string());
            }
            Ok(MutationReply::Uncommitted { .. }) => {
                let _ = cancel_surface_operation(prepared);
                return Err("ACP interaction response did not commit".to_string());
            }
            Err(error) => {
                let _ = cancel_surface_operation(prepared);
                return Err(format!("ACP interaction response failed: {error:?}"));
            }
        }
    }
    emit_surface_event(session_id, note_tx, event, &mut prepared.tool_outputs);
    Ok(())
}

fn tool_call_title(request: &crate::runtime_surface::SurfaceToolRequest) -> String {
    request
        .target
        .as_ref()
        .map(|target| format!("{}: {}", request.name.as_str(), target.as_str()))
        .unwrap_or_else(|| request.name.as_str().to_string())
}

fn tool_kind(action: SurfaceToolAction) -> ToolKind {
    match action {
        SurfaceToolAction::Read => ToolKind::Read,
        SurfaceToolAction::Write => ToolKind::Edit,
        SurfaceToolAction::Network => ToolKind::Fetch,
        SurfaceToolAction::Agent => ToolKind::Think,
        SurfaceToolAction::Shell => ToolKind::Execute,
    }
}

fn cancel_surface_operation(prepared: &PreparedSurfacePrompt) -> Result<(), String> {
    match prepared
        .client
        .cancel_operation(SurfaceRequestId::new(), prepared.operation_id.clone())
        .map_err(|error| format!("ACP surface cancellation failed: {error:?}"))?
    {
        MutationReply::Committed { .. } | MutationReply::Deferred { .. } => Ok(()),
        MutationReply::Uncommitted { .. } => {
            Err("ACP surface cancellation did not commit".to_string())
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for OrcaAcpAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
        {
            let mut state = self.state.borrow_mut();
            if state.client_capabilities.is_some() || !state.sessions.is_empty() {
                return Err(Error::invalid_request().data("ACP session is already initialized"));
            }
            state.client_capabilities =
                Some(AcpClientCapabilityProfile::from(&args.client_capabilities));
        }
        Ok(
            InitializeResponse::new(ProtocolVersion::V1)
                .agent_capabilities(
                    AgentCapabilities::new()
                        .load_session(true)
                        .mcp_capabilities(McpCapabilities::new().sse(true))
                        .session_capabilities(SessionCapabilities::new().additional_directories(
                            SessionAdditionalDirectoriesCapabilities::new(),
                        )),
                )
                .agent_info(
                    Implementation::new("orca", self.base_config.app_version.clone())
                        .title("Orca".to_string()),
                ),
        )
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> Result<AuthenticateResponse, Error> {
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        self.negotiated_client_capabilities()?;
        let config = self
            .build_session_config(args.cwd, args.mcp_servers, args.additional_directories)
            .map_err(|message| Error::invalid_params().data(message))?;
        let surface_host = self.surface_host.clone();
        let thread =
            tokio::task::spawn_blocking(move || surface_host.start_thread(config, "ACP session"))
                .await
                .map_err(Error::into_internal_error)?
                .map_err(Error::into_internal_error)?;
        let surface = thread
            .acp_surface()
            .ok_or_else(|| Error::internal_error().data("ACP surface unavailable"))?;

        let session_id: SessionId = match thread.session_id() {
            Some(id) => SessionId::new(id),
            None => SessionId::new(uuid::Uuid::new_v4().to_string()),
        };

        self.state.borrow_mut().sessions.insert(
            session_id.clone(),
            SessionEntry {
                surface,
                prompt_binding: None,
                next_prompt_seq: 1,
            },
        );
        Ok(NewSessionResponse::new(session_id))
    }

    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        self.negotiated_client_capabilities()?;
        if self.state.borrow().sessions.contains_key(&args.session_id) {
            return Err(Error::invalid_params().data("ACP session is already loaded"));
        }
        let selector = args.session_id.to_string();
        let config = self
            .build_session_config(args.cwd, args.mcp_servers, args.additional_directories)
            .map_err(|message| Error::invalid_params().data(message))?;
        let surface_host = self.surface_host.clone();
        let thread = tokio::task::spawn_blocking(move || {
            surface_host.load_recorded_thread(config, "ACP session", &selector)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(|error| match error {
            RuntimeSurfaceRecordedThreadLoadError::CwdMismatch => {
                Error::invalid_params().data("ACP load cwd does not match saved session")
            }
            RuntimeSurfaceRecordedThreadLoadError::Runtime(error) => {
                Error::into_internal_error(error)
            }
        })?;
        let surface = thread
            .acp_surface()
            .ok_or_else(|| Error::internal_error().data("ACP surface unavailable"))?;
        let replay_surface = surface.clone();
        let session_id = args.session_id.clone();
        let note_tx = self.note_tx.clone();
        tokio::task::spawn_blocking(move || {
            replay_surface_snapshot(&replay_surface, &session_id, &note_tx)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(|message| Error::internal_error().data(message))?;

        self.state.borrow_mut().sessions.insert(
            args.session_id.clone(),
            SessionEntry {
                surface,
                prompt_binding: None,
                next_prompt_seq: 1,
            },
        );
        Ok(LoadSessionResponse::new())
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse, Error> {
        let admitted = self.admit_prompt(args, None).await?;
        self.complete_prompt(admitted).await
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        if let Some(bridge) = self.client_bridge.as_ref() {
            bridge.cancel_session(&args.session_id);
            bridge.wait_for_capability_writes(&args.session_id).await;
        }
        let session_id = NonEmptyText::try_new(args.session_id.to_string()).map_err(|error| {
            Error::invalid_params().data(format!("invalid ACP session id: {error}"))
        })?;
        loop {
            let binding = {
                let state = self.state.borrow();
                match state.sessions.get(&args.session_id) {
                    Some(SessionEntry {
                        prompt_binding:
                            Some(AcpPromptBinding::Bound {
                                client,
                                inbound_seq,
                                ..
                            }),
                        ..
                    }) => Some(Ok((client.clone(), *inbound_seq))),
                    Some(SessionEntry {
                        prompt_binding: Some(AcpPromptBinding::Decoded { ready }),
                        ..
                    }) => Some(Err(ready.clone())),
                    Some(_) | None => None,
                }
            };
            match binding {
                Some(Ok((client, inbound_seq))) => {
                    let session_id = session_id.clone();
                    tokio::task::spawn_blocking(move || {
                        client.cancel_acp_prompt_binding(
                            SurfaceRequestId::new(),
                            session_id,
                            inbound_seq,
                        )
                    })
                    .await
                    .map_err(Error::into_internal_error)?
                    .map_err(|error| match error {
                        SurfaceClientCommandError::RuntimeUnavailable => Error::internal_error()
                            .data("ACP cancel could not commit runtime state"),
                        SurfaceClientCommandError::Unauthorized => Error::internal_error()
                            .data("ACP cancel was rejected by runtime ownership"),
                    })?;
                    break;
                }
                Some(Err(ready)) => ready.notified().await,
                None => break,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::runtime_host::{
        GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
        ThreadOperationOutcome,
    };
    use crate::runtime_surface::{
        SurfaceEvent, SurfaceToolRequest, SurfaceToolResult, SurfaceToolTerminal,
        ToolInvocationStarted, ToolTerminalSource,
    };
    use crate::thread::RuntimeThread;
    use agent_client_protocol::{
        EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
    };
    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::thread_identity::TurnId;

    fn test_absolute_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    struct CompleteImmediatelyExecutor;

    impl ThreadOperationExecutor for CompleteImmediatelyExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            Ok(RunStatus::Success.into())
        }
    }

    fn permission_request(session_id: &str, tool_call_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new(session_id),
            ToolCallUpdate::new(ToolCallId::new(tool_call_id), ToolCallUpdateFields::new()),
            Vec::new(),
        )
    }

    fn attachment_id(seed: u8) -> SurfaceAttachmentId {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        SurfaceAttachmentId::try_from_bytes(bytes).expect("valid UUIDv7 attachment id")
    }

    #[test]
    fn prompt_content_decodes_supported_blocks_in_original_order() {
        use agent_client_protocol::{
            EmbeddedResource, EmbeddedResourceResource, ResourceLink, TextResourceContents,
        };

        let blocks = vec![
            ContentBlock::from("first".to_string()),
            ContentBlock::ResourceLink(
                ResourceLink::new("notes", "file:///workspace/notes.txt")
                    .description("notes description")
                    .mime_type("text/plain"),
            ),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new("embedded", "file:///workspace/context.txt")
                        .mime_type("text/markdown"),
                ),
            )),
            ContentBlock::from("last".to_string()),
        ];

        let decoded = decode_prompt_content(
            &blocks,
            AcpClientCapabilityProfile {
                read_text_file: true,
                ..AcpClientCapabilityProfile::negotiated_for_test()
            },
        )
        .expect("supported ACP prompt content");
        let decoded = decoded.blocks.as_slice();
        assert_eq!(decoded.len(), 4);
        assert!(matches!(
            &decoded[0],
            SurfaceInputRequestBlock::Text { text } if text.as_str() == "first"
        ));
        assert!(matches!(
            &decoded[1],
            SurfaceInputRequestBlock::ResourceLink {
                uri,
                name,
                description: Some(description),
                mime: Some(mime),
            } if uri.as_str() == "file:///workspace/notes.txt"
                && name.as_str() == "notes"
                && description.as_str() == "notes description"
                && mime.as_str() == "text/plain"
        ));
        assert!(matches!(
            &decoded[2],
            SurfaceInputRequestBlock::EmbeddedText {
                uri,
                mime,
                text,
                ..
            } if uri.as_str() == "file:///workspace/context.txt"
                && mime.as_str() == "text/markdown"
                && text.as_str() == "embedded"
        ));
        assert!(matches!(
            &decoded[3],
            SurfaceInputRequestBlock::Text { text } if text.as_str() == "last"
        ));
    }

    #[test]
    fn prompt_content_rejects_binary_blocks_before_surface_reservation() {
        use agent_client_protocol::ImageContent;

        let error = decode_prompt_content(
            &[ContentBlock::Image(ImageContent::new(
                "base64-payload",
                "image/png",
            ))],
            AcpClientCapabilityProfile::negotiated_for_test(),
        )
        .expect_err("image content lacks a frozen runtime mapping");
        assert!(error.contains("unsupported ACP prompt content block: image"));
    }

    #[test]
    fn acp_session_declarations_map_supported_mcp_and_canonical_roots() {
        let cwd = tempfile::tempdir().expect("cwd");
        let extra = tempfile::tempdir().expect("additional directory");
        let mut base = test_run_config(cwd.path().to_path_buf());
        base.mcp_servers.clear();
        base.additional_working_directories.clear();
        base.runtime_workspace_roots = None;
        let stdio_command = test_absolute_path("example-mcp-server");

        let config = build_acp_session_config(
            base,
            cwd.path().to_path_buf(),
            vec![
                McpServer::Stdio(
                    McpServerStdio::new("stdio", stdio_command.clone())
                        .args(vec!["--stdio".to_string()])
                        .env(vec![EnvVariable::new("TOKEN", "secret")]),
                ),
                McpServer::Sse(
                    McpServerSse::new("sse", "https://example.test/mcp")
                        .headers(vec![HttpHeader::new("Authorization", "Bearer secret")]),
                ),
            ],
            vec![extra.path().to_path_buf()],
        )
        .expect("supported session declarations");

        assert_eq!(
            config.runtime_workspace_roots,
            Some(vec![cwd.path().to_path_buf(), extra.path().to_path_buf()])
        );
        assert_eq!(config.additional_working_directories.len(), 1);
        assert_eq!(config.additional_working_directories[0].path, extra.path());
        assert_eq!(config.additional_working_directories[0].source, "acp");
        assert_eq!(config.mcp_servers.len(), 2);
        assert_eq!(config.mcp_servers[0].name, "stdio");
        assert_eq!(
            config.mcp_servers[0].command.as_deref(),
            stdio_command.to_str()
        );
        assert_eq!(
            config.mcp_servers[0].env.get("TOKEN").map(String::as_str),
            Some("secret")
        );
        assert_eq!(config.mcp_servers[1].name, "sse");
        assert_eq!(
            config.mcp_servers[1].url.as_deref(),
            Some("https://example.test/mcp")
        );
        assert_eq!(
            config.mcp_servers[1]
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer secret")
        );
    }

    #[test]
    fn acp_session_declarations_reject_unsupported_or_ambiguous_scope() {
        let cwd = tempfile::tempdir().expect("cwd");
        let relative = PathBuf::from("relative-root");
        let absolute_command = test_absolute_path("mcp-server");
        let base = test_run_config(cwd.path().to_path_buf());
        assert!(
            build_acp_session_config(
                base.clone(),
                cwd.path().to_path_buf(),
                Vec::new(),
                vec![relative],
            )
            .expect_err("relative additional directory must fail")
            .contains("additional directory")
        );
        assert!(
            build_acp_session_config(
                base.clone(),
                cwd.path().to_path_buf(),
                vec![McpServer::Http(McpServerHttp::new(
                    "http",
                    "https://example.test/mcp",
                ))],
                Vec::new(),
            )
            .expect_err("HTTP MCP transport is not supported")
            .contains("HTTP MCP transport")
        );
        assert!(
            build_acp_session_config(
                base.clone(),
                cwd.path().to_path_buf(),
                vec![McpServer::Stdio(McpServerStdio::new(
                    "relative-command",
                    "mcp-server",
                ))],
                Vec::new(),
            )
            .expect_err("relative executable must fail")
            .contains("absolute command")
        );
        assert!(
            build_acp_session_config(
                base.clone(),
                cwd.path().to_path_buf(),
                vec![
                    McpServer::Stdio(McpServerStdio::new("a-b", absolute_command.clone(),)),
                    McpServer::Stdio(McpServerStdio::new("a_b", absolute_command)),
                ],
                Vec::new(),
            )
            .expect_err("canonical MCP names must be unique")
            .contains("duplicate ACP MCP server")
        );
        assert!(
            build_acp_session_config(
                base,
                cwd.path().to_path_buf(),
                vec![McpServer::Sse(
                    McpServerSse::new("headers", "https://example.test/mcp").headers(vec![
                        HttpHeader::new("Authorization", "first"),
                        HttpHeader::new("authorization", "second"),
                    ]),
                )],
                Vec::new(),
            )
            .expect_err("header names are case-insensitive")
            .contains("duplicate header name")
        );
    }

    #[test]
    fn terminal_mapping_preserves_only_exact_standard_stop_reasons() {
        use crate::runtime_surface::{
            NotAdmittedReason, OperationBudget, SurfaceShutdownReason, TurnRequestBudgetScope,
        };

        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::BudgetExhausted {
                budget: OperationBudget::ModelTokens {
                    limit: Some(100),
                    observed: Some(100),
                },
            }),
            Ok(StopReason::MaxTokens)
        );
        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::BudgetExhausted {
                budget: OperationBudget::TurnRequests {
                    scope: TurnRequestBudgetScope::AgentLoop,
                    limit: 8,
                    observed: 8,
                },
            }),
            Ok(StopReason::MaxTurnRequests)
        );
        assert!(
            terminal_to_stop_reason(&OperationTerminal::BudgetExhausted {
                budget: OperationBudget::TurnRequests {
                    scope: TurnRequestBudgetScope::Subagent,
                    limit: 4,
                    observed: 4,
                },
            })
            .expect_err("subagent budget is not the ACP agent-loop turn limit")
            .contains("Subagent")
        );
        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::CancelledBeforeAdmission,
            }),
            Ok(StopReason::Cancelled)
        );
        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::HostShutdown,
            }),
            Ok(StopReason::Cancelled)
        );
    }

    #[test]
    fn standard_tool_approval_options_never_fabricate_persistent_scope() {
        let options = standard_tool_approval_options();
        assert_eq!(
            options
                .iter()
                .map(|option| option.option_id.to_string())
                .collect::<Vec<_>>(),
            vec!["allow_once".to_string(), "reject_once".to_string()]
        );
        let error = permission_answer(
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new("allow_always"),
            )),
            AcpPermissionTarget::ToolApproval,
        )
        .expect_err("unadvertised persistent grant must be rejected");
        assert!(error.contains("unknown ACP permission option"));
    }

    #[test]
    fn standard_interaction_capabilities_only_advertise_exact_tool_approval() {
        assert_eq!(
            standard_interaction_capabilities(),
            std::collections::BTreeSet::from([SurfaceInteractionKind::ToolApproval])
        );
    }

    #[test]
    fn standard_acp_ignores_ungranted_and_extension_only_interactions() {
        let acp = attachment_id(1);
        let other = attachment_id(2);
        let acp_route = SurfaceInteractionRoute::Exclusive {
            epoch: crate::surface::ResponseRouteEpoch::try_new(1).expect("valid route epoch"),
            attachment_id: acp.clone(),
        };
        let other_route = SurfaceInteractionRoute::Exclusive {
            epoch: crate::surface::ResponseRouteEpoch::try_new(1).expect("valid route epoch"),
            attachment_id: other,
        };

        assert!(standard_acp_routes_interaction(
            &acp,
            SurfaceInteractionKind::ToolApproval,
            &acp_route,
        ));
        assert!(!standard_acp_routes_interaction(
            &acp,
            SurfaceInteractionKind::ToolApproval,
            &other_route,
        ));
        assert!(!standard_acp_routes_interaction(
            &acp,
            SurfaceInteractionKind::PermissionRequest,
            &acp_route,
        ));
    }

    #[test]
    fn permission_cancel_before_registration_is_not_lost() {
        let (bridge, _requests) = AcpClientBridge::new();
        let session_id = SessionId::new("cancel-before-register");
        bridge.cancel_session(&session_id);

        let result =
            bridge.request_permission(permission_request("cancel-before-register", "tool-1"));
        assert_eq!(result, Err(AcpPermissionWaitError::Cancelled));
    }

    #[test]
    fn permission_cancel_wakes_waiter_and_does_not_reuse_key() {
        let (bridge, mut requests) = AcpClientBridge::new();
        let session_id = SessionId::new("cancel-waiter");
        let waiter_bridge = Arc::clone(&bridge);
        let waiter = std::thread::spawn(move || {
            waiter_bridge.request_permission(permission_request("cancel-waiter", "tool-1"))
        });
        let request = requests
            .blocking_recv()
            .expect("permission request is queued");
        assert!(bridge.is_pending(&request.key));
        bridge.cancel_session(&session_id);
        assert_eq!(
            waiter.join().expect("permission waiter joins"),
            Err(AcpPermissionWaitError::Cancelled)
        );
        assert!(!bridge.is_pending(&request.key));
    }

    #[test]
    fn permission_response_releases_exact_waiter() {
        let (bridge, mut requests) = AcpClientBridge::new();
        let waiter_bridge = Arc::clone(&bridge);
        let waiter = std::thread::spawn(move || {
            waiter_bridge.request_permission(permission_request("respond", "tool-1"))
        });
        let request = requests
            .blocking_recv()
            .expect("permission request is queued");
        bridge.complete_permission(
            &request.key,
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_once")),
            )),
        );
        let response = waiter
            .join()
            .expect("permission waiter joins")
            .expect("permission response succeeds");
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(_)
        ));
    }

    #[test]
    fn lost_subscription_reconciles_durable_terminal_without_cancelling_operation() {
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteImmediatelyExecutor)).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let surface_host = host.surface_handle().bind_new_connection();
        let config = test_run_config(cwd.path().to_path_buf());
        let thread =
            std::thread::spawn(move || surface_host.start_thread(config, "ACP reconcile").unwrap())
                .join()
                .unwrap();
        let surface = thread.acp_surface().expect("ACP surface");
        let input = decode_prompt_content(
            &[ContentBlock::from("complete".to_string())],
            AcpClientCapabilityProfile::negotiated_for_test(),
        )
        .expect("decode typed prompt");
        let mut prepared = prepare_surface_prompt(
            &surface,
            &SessionId::new("reconcile"),
            input,
            1,
            AcpClientCapabilityProfile::negotiated_for_test(),
            None,
        )
        .unwrap();
        let operation_id = prepared.operation_id.clone();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "operation did not reach a terminal event"
            );
            let Some(item) = prepared
                .subscription
                .recv_timeout(Duration::from_millis(50))
            else {
                continue;
            };
            let SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            if batch.events.as_slice().iter().any(|envelope| {
                matches!(
                    &envelope.event,
                    SurfaceEvent::Operation(OperationPatch::Terminal { record })
                        if record.operation_id == operation_id
                )
            }) {
                break;
            }
        }

        let error = reconcile_lost_subscription(&mut prepared, "gap").unwrap_err();
        assert!(error.contains("after durable terminal EndTurn"));
        drop(prepared);

        let snapshot = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Acp,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
            _ => panic!("unexpected snapshot attachment"),
        };
        let terminal = snapshot
            .operation_history
            .iter()
            .chain(snapshot.foreground_operation.iter())
            .find(|operation| operation.operation_id == operation_id)
            .and_then(|operation| operation.terminal.as_ref())
            .expect("durable operation terminal");
        assert!(matches!(
            terminal.terminal,
            OperationTerminal::Succeeded { .. }
        ));
        host.shutdown().unwrap();
    }

    #[test]
    fn sealed_subscription_reconciles_terminal_from_retained_runtime_snapshot() {
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteImmediatelyExecutor)).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let surface_host = host.surface_handle().bind_new_connection();
        let config = test_run_config(cwd.path().to_path_buf());
        let thread =
            std::thread::spawn(move || surface_host.start_thread(config, "ACP sealed").unwrap())
                .join()
                .unwrap();
        let surface = thread.acp_surface().expect("ACP surface");
        let input = decode_prompt_content(
            &[ContentBlock::from("complete".to_string())],
            AcpClientCapabilityProfile::negotiated_for_test(),
        )
        .expect("decode typed prompt");
        let mut prepared = prepare_surface_prompt(
            &surface,
            &SessionId::new("sealed"),
            input,
            1,
            AcpClientCapabilityProfile::negotiated_for_test(),
            None,
        )
        .unwrap();
        let operation_id = prepared.operation_id.clone();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "operation did not reach terminal before shutdown"
            );
            let Some(item) = prepared
                .subscription
                .recv_timeout(Duration::from_millis(50))
            else {
                continue;
            };
            let SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            if batch.events.as_slice().iter().any(|envelope| {
                matches!(
                    &envelope.event,
                    SurfaceEvent::Operation(OperationPatch::Terminal { record })
                        if record.operation_id == operation_id
                )
            }) {
                break;
            }
        }

        host.shutdown().unwrap();
        let sealed = prepared
            .subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("runtime shutdown seals the subscription");
        assert!(matches!(
            sealed,
            SurfaceSubscriptionItem::Sealed {
                reason: crate::runtime_surface::SurfaceSubscriptionSealReason::HostShutdown,
            }
        ));
        let error = reconcile_lost_subscription(&mut prepared, "sealed").unwrap_err();
        assert!(error.contains("after durable terminal EndTurn"));
    }

    #[test]
    fn tool_events_project_as_typed_acp_updates() {
        let request = SurfaceToolRequest {
            tool_call_id: crate::runtime_surface::SurfaceToolCallId::try_new("tool-typed").unwrap(),
            source_response_id: None,
            turn_id: TurnId::new(),
            name: NonEmptyText::try_new("shell").unwrap(),
            action: SurfaceToolAction::Shell,
            target: Some(DisplayText::new("cargo test")),
            raw_arguments: DisplayText::new(r#"{"command":"cargo test"}"#),
            arguments_digest: crate::runtime_surface::Sha256Digest::new([0; 32]),
        };
        let (note_tx, mut note_rx) = mpsc::channel(8);
        let note_tx = AcpNotificationSender::Buffered(note_tx);
        let mut tool_outputs = HashMap::new();
        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::Requested {
                request: request.clone(),
            }),
            &mut tool_outputs,
        );
        let update = note_rx.try_recv().expect("tool request update");
        match update.update {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.kind, ToolKind::Execute);
                assert_eq!(call.status, ToolCallStatus::Pending);
                assert_eq!(call.title, "shell: cargo test");
                assert!(call.raw_input.is_some());
            }
            other => panic!("expected typed tool call, got {other:?}"),
        }

        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::OutputDelta {
                tool_call_id: request.tool_call_id.clone(),
                offset: crate::runtime_surface::ByteOffset::new(0),
                chunk: DisplayText::new("done"),
            }),
            &mut tool_outputs,
        );
        let _ = note_rx.try_recv().expect("tool output update");
        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::OutputDelta {
                tool_call_id: request.tool_call_id.clone(),
                offset: crate::runtime_surface::ByteOffset::new(0),
                chunk: DisplayText::new("done"),
            }),
            &mut tool_outputs,
        );
        assert!(
            note_rx.try_recv().is_err(),
            "duplicate output must be suppressed"
        );
        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::Completed {
                result: SurfaceToolResult {
                    tool_call_id: request.tool_call_id,
                    name: request.name,
                    terminal: SurfaceToolTerminal {
                        kind: SurfaceToolResultKind::Success,
                        source: ToolTerminalSource::Observed,
                        invocation_started: ToolInvocationStarted::Yes,
                    },
                    output: Some(DisplayText::new("done")),
                    error: None,
                    exit_code: Some(0),
                    truncated: false,
                    file_change: None,
                },
            }),
            &mut tool_outputs,
        );
        let update = note_rx.try_recv().expect("tool completion update");
        match update.update {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
            }
            other => panic!("expected typed tool completion, got {other:?}"),
        }
    }

    fn test_run_config(cwd: PathBuf) -> RunConfig {
        // Every test resolves ORCA_HOME to the process-wide isolated home so
        // parallel tests never contend with live `orca` processes or each
        // other's deleted temp dirs; an explicitly provided home (recovery
        // child fixture) is preserved.
        let _ = crate::history::claim_isolated_test_orca_home_if_unset();
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
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
            history_mode: HistoryMode::Record,
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
}
