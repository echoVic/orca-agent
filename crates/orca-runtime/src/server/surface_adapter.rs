use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use orca_core::thread_identity::TurnId;
use sha2::{Digest, Sha256};

use super::direct_interaction_adapter::{
    JsonlDirectInteractionAdapter, JsonlDirectInteractionKind, JsonlDirectInteractionRoute,
};
use super::opaque_permission_router::{
    JsonlOpaquePermissionRouter, JsonlPermissionRoute, JsonlRetiredRequestOwner,
};
use super::{
    PermissionProfileOverride, ServerThreadSubmissionContext, ServerThreadView,
    apply_permission_override,
};
use crate::protocol::ServerEvent;
use crate::runtime_host::{
    HostedOperationWriter, RuntimeHost, RuntimeHostError, RuntimeThreadStartRequest,
};
use crate::surface::{
    AssistantChannel, AssistantPatch, AttachResult, DetachRequest, DisplayText, FreshAttachRequest,
    InteractionPatch, LegacyTurnId, MutationReply, NonEmptyVec, OperationIngressCorrelation,
    OperationKind, OperationPatch, OperationRequestIntent, OperationSettingsPreparation,
    OperationTerminal, ReplayabilityRequest, RuntimeSettingsPatch, RuntimeSurfaceClientHandle,
    RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, RuntimeSurfaceThreadHandle, Sha256Digest,
    SurfaceAdmissionLeaseId, SurfaceAssistantStream, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceCommitBatch, SurfaceEvent, SurfaceInputRequest, SurfaceInputRequestBlock,
    SurfaceInteractionKind, SurfaceInteractionRequest, SurfaceInteractionRoute, SurfaceOperationId,
    SurfaceRequestId, SurfaceScope, SurfaceSubscriptionItem, SurfaceSubscriptionReceiver,
    SurfaceToolRequest, SurfaceToolResultKind, SurfaceWorkflowResultStatus, ToolPatch,
    ToolTerminalSource, UncommittedMutation, WorkflowPatch,
};
use crate::thread_store::{
    SortDirection, StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchPage,
    StoredThreadSummaryPage, StoredThreadTurnPage, ThreadListFilters, ThreadMetadataPatch,
    ThreadSortKey, TurnItemsView,
};

#[derive(Clone)]
pub(crate) struct JsonlInteractionTransport {
    permissions: JsonlOpaquePermissionRouter<JsonlPermissionRoute>,
    direct: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>,
}

impl JsonlInteractionTransport {
    pub(super) fn new(
        permissions: JsonlOpaquePermissionRouter<JsonlPermissionRoute>,
        direct: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>,
    ) -> Self {
        Self {
            permissions,
            direct,
        }
    }
}

pub struct JsonlSurfaceAdapter {
    host: Option<RuntimeHost>,
    surface_host: RuntimeSurfaceHostHandle,
    threads: HashMap<String, JsonlThreadBinding>,
    ephemeral_threads: Arc<Mutex<HashMap<String, RuntimeSurfaceThreadHandle>>>,
    transport_turns: Vec<JsonlTransportTurn>,
}

struct JsonlThreadBinding {
    thread: RuntimeSurfaceThreadHandle,
}

pub(crate) struct PreparedJsonlTurn {
    thread_id: String,
    expected_turn_id: Option<TurnId>,
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: SurfaceOperationId,
    admission_lease_id: SurfaceAdmissionLeaseId,
    subscription: SurfaceSubscriptionReceiver,
    interactions: Option<JsonlInteractionTransport>,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    ephemeral_thread: Option<EphemeralRuntimeThread>,
    clean_eof_policy: JsonlCleanEofPolicy,
}

pub(crate) struct JsonlTransportTurn {
    thread_id: String,
    turn_id: TurnId,
    clean_eof_policy: JsonlCleanEofPolicy,
    worker: Option<std::thread::JoinHandle<io::Result<()>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonlCleanEofPolicy {
    CancelOnConnectionClose,
    CompleteEphemeralOneShot,
}

pub(crate) trait JsonlSurfaceOutput: HostedOperationWriter {
    fn write_server_event(&mut self, _event: ServerEvent) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "JSONL surface output does not support direct protocol events",
        ))
    }

    fn supports_direct_server_events(&self) -> bool {
        false
    }
}

struct EphemeralRuntimeThread {
    thread: Option<RuntimeSurfaceThreadHandle>,
    thread_id: String,
    registry: Arc<Mutex<HashMap<String, RuntimeSurfaceThreadHandle>>>,
}

impl EphemeralRuntimeThread {
    fn register(
        thread: RuntimeSurfaceThreadHandle,
        registry: Arc<Mutex<HashMap<String, RuntimeSurfaceThreadHandle>>>,
    ) -> io::Result<Self> {
        let thread_id = thread.thread_id().to_string();
        registry
            .lock()
            .map_err(|_| io::Error::other("ephemeral runtime registry lock poisoned"))?
            .insert(thread_id.clone(), thread.clone());
        Ok(Self {
            thread: Some(thread),
            thread_id,
            registry,
        })
    }
}

impl Drop for EphemeralRuntimeThread {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.remove(&self.thread_id);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.shutdown();
        }
    }
}

impl JsonlSurfaceAdapter {
    pub fn start() -> io::Result<Self> {
        let host = RuntimeHost::start().map_err(runtime_host_error)?;
        let surface_host = host.surface_handle().bind_new_connection();
        Ok(Self {
            host: Some(host),
            surface_host,
            threads: HashMap::new(),
            ephemeral_threads: Arc::new(Mutex::new(HashMap::new())),
            transport_turns: Vec::new(),
        })
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.threads.clear();
        let result = self
            .host
            .take()
            .map(|host| host.shutdown().map_err(runtime_host_error))
            .unwrap_or(Ok(()));
        for turn in &mut self.transport_turns {
            let _ = turn.wait_terminal();
        }
        self.transport_turns.clear();
        result
    }

    pub(crate) fn connection_id(&self) -> Option<crate::surface::SurfaceConnectionId> {
        self.surface_host.connection_id().cloned()
    }

    pub fn start_thread(&mut self, config: &RunConfig) -> io::Result<String> {
        let config = jsonl_thread_config(config);
        self.start_record(config, "(empty prompt)")
    }

    pub fn resume_thread(&mut self, config: &RunConfig, thread_id: &str) -> io::Result<String> {
        self.resume_thread_with_permissions(config, thread_id, PermissionProfileOverride::default())
    }

    pub fn fork_thread(&mut self, config: &RunConfig, thread_id: &str) -> io::Result<String> {
        self.fork_thread_with_permissions(config, thread_id, PermissionProfileOverride::default())
    }

    pub fn resume_thread_with_permissions(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<String> {
        if self.threads.contains_key(thread_id) {
            if !permissions.is_empty() {
                self.persist_permission_override(thread_id, config, permissions)?;
            }
            return Ok(thread_id.to_string());
        }
        let mut config = config.clone();
        config.output_format = OutputFormat::Jsonl;
        config.history_mode = HistoryMode::Resume(thread_id.to_string());
        config.show_session_picker = false;
        config.desktop_notifications = false;
        apply_permission_override(&mut config, permissions);
        self.start_record(config, "(resumed prompt)")
    }

    pub fn fork_thread_with_permissions(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<String> {
        let binding = self.threads.get(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        let surface = binding
            .thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return Err(io::Error::other("JSONL fork source snapshot unavailable")),
        };
        let mut config = config.clone();
        apply_surface_settings_to_run_config(
            &mut config,
            &attachment.baseline.snapshot.settings.effective,
        )?;
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        config.output_format = OutputFormat::Jsonl;
        config.history_mode = HistoryMode::Fork(thread_id.to_string());
        config.show_session_picker = false;
        config.desktop_notifications = false;
        apply_permission_override(&mut config, permissions);
        self.start_record(config, "(empty prompt)")
    }

    fn start_record(&mut self, config: RunConfig, title: &str) -> io::Result<String> {
        let thread = self
            .surface_host
            .start_thread(config, title)
            .map_err(runtime_host_error)?;
        let thread_id = thread.thread_id().to_string();
        self.threads
            .insert(thread_id.clone(), JsonlThreadBinding { thread });
        Ok(thread_id)
    }

    pub fn has_thread(&self, thread_id: &str) -> bool {
        self.threads.contains_key(thread_id)
    }

    pub fn task_registry(&self, thread_id: &str) -> Option<crate::tasks::TaskRegistry> {
        self.threads
            .get(thread_id)
            .map(|binding| binding.thread.task_registry())
    }

    pub(crate) fn mcp_registry(&self, thread_id: &str) -> Option<orca_mcp::McpRegistry> {
        self.threads
            .get(thread_id)
            .map(|binding| binding.thread.mcp_registry())
            .or_else(|| {
                self.ephemeral_thread(thread_id)
                    .map(|thread| thread.mcp_registry())
            })
    }

    pub(crate) fn jsonl_surface(&self, thread_id: &str) -> Option<RuntimeSurfaceHandle> {
        self.threads
            .get(thread_id)
            .and_then(|binding| binding.thread.jsonl_surface())
            .or_else(|| {
                self.ephemeral_thread(thread_id)
                    .and_then(|thread| thread.jsonl_surface())
            })
    }

    pub fn prompt_queue(
        &self,
        thread_id: &str,
        action: crate::prompt_queue::PromptQueueAction,
    ) -> Result<
        crate::prompt_queue::PromptQueueSnapshot,
        crate::prompt_queue::PromptQueueMutationError,
    > {
        if let Some(binding) = self.threads.get(thread_id) {
            return binding.thread.prompt_queue(action);
        }
        self.ephemeral_thread(thread_id)
            .ok_or(crate::prompt_queue::PromptQueueMutationError::RuntimeUnavailable)?
            .prompt_queue(action)
    }

    fn ephemeral_thread(&self, thread_id: &str) -> Option<RuntimeSurfaceThreadHandle> {
        self.ephemeral_threads
            .lock()
            .ok()
            .and_then(|threads| threads.get(thread_id).cloned())
    }

    pub(crate) fn accepts_turn(&self, thread_id: &str, turn_id: &str) -> bool {
        self.thread_has_turn(thread_id, turn_id, true)
    }

    fn thread_has_turn(&self, thread_id: &str, turn_id: &str, active_only: bool) -> bool {
        let Some(surface) = self.jsonl_surface(thread_id) else {
            return false;
        };
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return false,
        };
        let snapshot = &attachment.baseline.snapshot;
        let accepts = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .any(|operation| {
                (!active_only || operation.terminal.is_none())
                    && match &operation.intent.origin {
                        crate::surface::OperationOrigin::JsonlThreadTurn {
                            legacy_turn_id, ..
                        } => legacy_turn_id.0.as_str() == turn_id,
                        crate::surface::OperationOrigin::JsonlStatelessSubmit { .. } => operation
                            .initial_logical_turn_id
                            .as_ref()
                            .is_some_and(|logical_turn_id| logical_turn_id.to_string() == turn_id),
                        _ => false,
                    }
            });
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        accepts
    }

    pub(crate) fn resolve_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        let mut thread_ids = self.control_thread_ids();
        thread_ids.sort();
        thread_ids
            .into_iter()
            .find(|thread_id| self.accepts_turn(thread_id, turn_id))
    }

    pub(crate) fn resolve_known_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        let mut thread_ids = self.control_thread_ids();
        thread_ids.sort();
        thread_ids
            .into_iter()
            .find(|thread_id| self.thread_has_turn(thread_id, turn_id, false))
    }

    pub(crate) fn list_sessions(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage> {
        self.surface_host.jsonl_list_sessions(
            cursor,
            limit,
            filters,
            sort_key,
            sort_direction,
            search_term,
        )
    }

    pub(crate) fn search_sessions(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage> {
        self.surface_host.jsonl_search_sessions(
            query,
            cursor,
            limit,
            include_archived,
            sort_key,
            sort_direction,
        )
    }

    pub(crate) fn read_session(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection> {
        if (include_messages || include_turns)
            && let Some(binding) = self.threads.get(thread_id)
        {
            return binding
                .thread
                .jsonl_read_live_projection(include_messages, include_turns)
                .map_err(runtime_host_error);
        }
        self.surface_host
            .jsonl_read_session(thread_id, include_messages, include_turns)
    }

    pub(crate) fn list_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage> {
        if let Some(binding) = self.threads.get(thread_id) {
            return binding
                .thread
                .jsonl_list_live_turns(cursor, limit, sort_direction, items_view)
                .map_err(runtime_host_error);
        }
        self.surface_host
            .jsonl_list_turns(thread_id, cursor, limit, sort_direction, items_view)
    }

    pub(crate) fn list_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage> {
        if let Some(binding) = self.threads.get(thread_id) {
            return binding
                .thread
                .jsonl_list_live_items(turn_id, cursor, limit, sort_direction)
                .map_err(runtime_host_error);
        }
        self.surface_host
            .jsonl_list_items(thread_id, turn_id, cursor, limit, sort_direction)
    }

    pub(crate) fn update_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<()> {
        self.surface_host
            .jsonl_update_session_metadata(thread_id, patch)
    }

    fn control_turn_inner(
        &self,
        thread_id: Option<&str>,
        turn_id: &str,
        action: crate::surface::JsonlTurnControlAction,
        preferred_client: Option<RuntimeSurfaceClientHandle>,
    ) -> io::Result<crate::surface::JsonlTurnControlResult> {
        let candidate_ids = match thread_id {
            Some(thread_id) => vec![thread_id.to_string()],
            None => {
                let mut ids = self.control_thread_ids();
                ids.sort();
                ids
            }
        };
        let legacy_turn_id = LegacyTurnId(DisplayText::new(turn_id));
        for candidate_id in candidate_ids {
            let Some(surface) = self.jsonl_surface(&candidate_id) else {
                continue;
            };
            let mut transient_attachment = None;
            let client = if let Some(client) = preferred_client
                .as_ref()
                .filter(|client| client.thread_id() == surface.thread_id())
            {
                client.clone()
            } else {
                let attachment = match surface.attach_fresh(FreshAttachRequest {
                    request_id: SurfaceRequestId::new(),
                    role: SurfaceAttachmentRole::Jsonl,
                    requested_capabilities: BTreeSet::from([
                        SurfaceCapability::ReadSnapshot,
                        SurfaceCapability::ControlBoundOperation,
                    ]),
                    interaction_capabilities: BTreeSet::new(),
                }) {
                    AttachResult::FreshAttached { attachment } => attachment,
                    _ => {
                        return Err(io::Error::other("JSONL control surface attach failed"));
                    }
                };
                let client = attachment.client.clone();
                transient_attachment = Some(attachment);
                client
            };
            let result = self
                .surface_host
                .control_jsonl_turn(
                    client,
                    SurfaceRequestId::new(),
                    Some(surface.thread_id().clone()),
                    legacy_turn_id.clone(),
                    action.clone(),
                )
                .map_err(|error| io::Error::other(format!("JSONL turn control failed: {error:?}")));
            if let Some(attachment) = transient_attachment {
                let client = attachment.client;
                let keep_attached = matches!(
                    result.as_ref(),
                    Ok(crate::surface::JsonlTurnControlResult::Resolved {
                        mutation: MutationReply::Committed { value, .. }
                    }) if value.echo.status
                        == crate::surface::JsonlResolvedTurnControlStatus::Resumed
                );
                if !keep_attached {
                    let _ = surface.detach(
                        &client,
                        DetachRequest {
                            request_id: SurfaceRequestId::new(),
                        },
                    );
                }
            }
            let result = result?;
            if thread_id.is_some()
                || matches!(
                    result,
                    crate::surface::JsonlTurnControlResult::Resolved { .. }
                )
            {
                return Ok(result);
            }
        }
        Ok(crate::surface::JsonlTurnControlResult::Idle {
            request_id: SurfaceRequestId::new(),
            echo: crate::surface::JsonlIdleTurnControlWireEcho {
                legacy_turn_id,
                action: match action {
                    crate::surface::JsonlTurnControlAction::Interrupt => {
                        crate::surface::JsonlTurnControlWireAction::Interrupt
                    }
                    crate::surface::JsonlTurnControlAction::Resume => {
                        crate::surface::JsonlTurnControlWireAction::Resume
                    }
                    crate::surface::JsonlTurnControlAction::Steer { .. } => {
                        crate::surface::JsonlTurnControlWireAction::Steer
                    }
                },
                status: crate::surface::JsonlIdleTurnControlStatus::Idle,
                legacy_input: None,
            },
        })
    }

    fn control_thread_ids(&self) -> Vec<String> {
        let mut ids = self.threads.keys().cloned().collect::<Vec<_>>();
        if let Ok(ephemeral) = self.ephemeral_threads.lock() {
            ids.extend(ephemeral.keys().cloned());
        }
        ids.sort();
        ids.dedup();
        ids
    }

    fn prepare_turn_inner(
        &self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &serde_json::Value,
        interactions: Option<JsonlInteractionTransport>,
    ) -> io::Result<PreparedJsonlTurn> {
        let binding = self.threads.get(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        let surface = binding
            .thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::ManageThreadSettings,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: jsonl_turn_interaction_capabilities(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return Err(io::Error::other("JSONL runtime surface attach failed")),
        };
        let baseline = &attachment.baseline.snapshot;
        let runtime_workspace_roots =
            permissions
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| {
                    baseline
                        .settings
                        .effective
                        .workspace_roots
                        .iter()
                        .map(|root| root.as_path().to_path_buf())
                        .collect()
                });
        let settings_preparation = settings_preparation(config, permissions, baseline)?;
        let turn_id = TurnId::new();
        let input = SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new(prompt),
            }])
            .map_err(|error| io::Error::other(error.to_string()))?,
        };
        let rpc_id_digest = Sha256Digest::new(Sha256::digest(rpc_id.to_string().as_bytes()).into());
        let intent = OperationRequestIntent {
            correlation: OperationIngressCorrelation::JsonlThreadTurn {
                rpc_id_digest,
                legacy_turn_id: LegacyTurnId(DisplayText::new(turn_id.to_string())),
            },
            kind: OperationKind::UserTurn,
            input: Some(input),
            replayability: ReplayabilityRequest::CaptureReplayableCapsule,
            settings_preparation,
        };
        let reserved = committed(
            attachment
                .client
                .reserve_operation(SurfaceRequestId::new(), intent),
            "JSONL surface reserve",
        )?;
        let operation_id = reserved.operation_id.clone();
        let admission_lease_id = reserved.lease.lease_id;

        let subscription = surface
            .claim_subscription(&attachment.subscription)
            .ok_or_else(|| io::Error::other("JSONL surface subscription unavailable"))?;

        Ok(PreparedJsonlTurn {
            thread_id: thread_id.to_string(),
            expected_turn_id: Some(turn_id),
            surface,
            client: attachment.client,
            operation_id,
            admission_lease_id,
            subscription,
            interactions,
            runtime_workspace_roots,
            ephemeral_thread: None,
            clean_eof_policy: JsonlCleanEofPolicy::CancelOnConnectionClose,
        })
    }

    pub(crate) fn prepare_stateless_turn_with_interactions(
        &self,
        config: &RunConfig,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &serde_json::Value,
        interactions: JsonlInteractionTransport,
    ) -> io::Result<PreparedJsonlTurn> {
        let mut config = config.clone();
        config.output_format = OutputFormat::Jsonl;
        config.history_mode = HistoryMode::Disabled;
        config.show_session_picker = false;
        config.desktop_notifications = false;
        apply_permission_override(&mut config, permissions.clone());

        let thread = self
            .surface_host
            .start_thread_with_request(
                RuntimeThreadStartRequest::new(config.clone(), "(stateless submit)")
                    .with_ephemeral_non_catalogued_one_shot(
                        crate::surface::FirstOperationCompletionPolicy::Terminal,
                    ),
            )
            .map_err(runtime_host_error)?;
        let ephemeral_thread =
            EphemeralRuntimeThread::register(thread, Arc::clone(&self.ephemeral_threads))?;
        let runtime_thread = ephemeral_thread
            .thread
            .as_ref()
            .expect("ephemeral runtime thread is retained until transport completion");
        let thread_id = runtime_thread.thread_id().to_string();
        let surface = runtime_thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL ephemeral runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: jsonl_turn_interaction_capabilities(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => {
                return Err(io::Error::other(
                    "JSONL ephemeral runtime surface attach failed",
                ));
            }
        };
        let baseline = &attachment.baseline.snapshot;
        let runtime_workspace_roots =
            permissions
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| {
                    baseline
                        .settings
                        .effective
                        .workspace_roots
                        .iter()
                        .map(|root| root.as_path().to_path_buf())
                        .collect()
                });
        let input = SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new(prompt),
            }])
            .map_err(|error| io::Error::other(error.to_string()))?,
        };
        let rpc_id_digest = Sha256Digest::new(Sha256::digest(rpc_id.to_string().as_bytes()).into());
        let intent = OperationRequestIntent {
            correlation: OperationIngressCorrelation::JsonlStatelessSubmit { rpc_id_digest },
            kind: OperationKind::UserTurn,
            input: Some(input),
            replayability: ReplayabilityRequest::NonReplayable {
                reason: crate::surface::NonReplayableReason::HistoryDisabled,
            },
            settings_preparation: settings_preparation(
                &config,
                PermissionProfileOverride::default(),
                baseline,
            )?,
        };
        let reserved = committed(
            attachment
                .client
                .reserve_operation(SurfaceRequestId::new(), intent),
            "JSONL stateless surface reserve",
        )?;
        let subscription = surface
            .claim_subscription(&attachment.subscription)
            .ok_or_else(|| io::Error::other("JSONL stateless surface subscription unavailable"))?;

        Ok(PreparedJsonlTurn {
            thread_id,
            expected_turn_id: None,
            surface,
            client: attachment.client,
            operation_id: reserved.operation_id,
            admission_lease_id: reserved.lease.lease_id,
            subscription,
            interactions: Some(interactions),
            runtime_workspace_roots,
            ephemeral_thread: Some(ephemeral_thread),
            clean_eof_policy: JsonlCleanEofPolicy::CompleteEphemeralOneShot,
        })
    }

    fn persist_permission_override(
        &self,
        thread_id: &str,
        config: &RunConfig,
        permissions: PermissionProfileOverride,
    ) -> io::Result<()> {
        let binding = self.threads.get(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        let surface = binding
            .thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ManageThreadSettings,
            ]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return Err(io::Error::other("JSONL settings surface attach failed")),
        };
        let preparation = settings_preparation(config, permissions, &attachment.baseline.snapshot)?;
        if let OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
            expected_settings_revision,
            patches,
            ..
        } = preparation
        {
            committed(
                attachment.client.update_settings(
                    SurfaceRequestId::new(),
                    expected_settings_revision,
                    patches,
                ),
                "JSONL settings update",
            )?;
        }
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        Ok(())
    }
}

fn apply_surface_settings_to_run_config(
    config: &mut RunConfig,
    settings: &crate::surface::SurfaceRuntimeSettings,
) -> io::Result<()> {
    config.cwd = Some(settings.cwd.as_path().to_path_buf());
    config.runtime_workspace_roots = Some(
        settings
            .workspace_roots
            .iter()
            .map(|root| root.as_path().to_path_buf())
            .collect(),
    );
    config.approval_mode = match settings.approval_mode {
        crate::surface::SurfaceApprovalMode::Suggest => {
            orca_core::approval_types::ApprovalMode::Suggest
        }
        crate::surface::SurfaceApprovalMode::AutoEdit => {
            orca_core::approval_types::ApprovalMode::AutoEdit
        }
        crate::surface::SurfaceApprovalMode::FullAuto => {
            orca_core::approval_types::ApprovalMode::FullAuto
        }
        crate::surface::SurfaceApprovalMode::Plan => orca_core::approval_types::ApprovalMode::Plan,
    };
    config.active_permission_profile = settings.active_permission_profile.as_ref().map(|profile| {
        orca_core::config::ActivePermissionProfile {
            id: profile.id.as_str().to_string(),
            extends: profile
                .extends
                .as_ref()
                .map(|value| value.as_str().to_string()),
        }
    });
    config.permission_rules = orca_core::approval_rules::PermissionRules {
        rules: settings
            .permission_rules
            .ordered_rules
            .iter()
            .map(|rule| {
                orca_core::approval_rules::PermissionRule::new(
                    rule.tool.as_str(),
                    rule.pattern.as_str(),
                    match rule.decision {
                        crate::surface::SurfacePermissionDecision::Allow => {
                            orca_core::approval_types::Decision::Allow
                        }
                        crate::surface::SurfacePermissionDecision::Prompt => {
                            orca_core::approval_types::Decision::Prompt
                        }
                        crate::surface::SurfacePermissionDecision::Deny => {
                            orca_core::approval_types::Decision::Deny
                        }
                    },
                )
            })
            .collect(),
    };
    config.additional_working_directories = settings
        .additional_working_directories
        .iter()
        .map(|directory| orca_core::config::AdditionalWorkingDirectory {
            path: directory.path.as_path().to_path_buf(),
            source: directory.source.as_str().to_string(),
        })
        .collect();
    config.reasoning_effort = match settings.reasoning_effort {
        crate::surface::SurfaceReasoningEffort::Low => orca_core::config::ReasoningEffort::Low,
        crate::surface::SurfaceReasoningEffort::High => orca_core::config::ReasoningEffort::High,
        crate::surface::SurfaceReasoningEffort::Max => orca_core::config::ReasoningEffort::Max,
        crate::surface::SurfaceReasoningEffort::Medium => {
            return Err(io::Error::other(
                "JSONL fork source uses an unsupported reasoning effort",
            ));
        }
    };
    config.model =
        orca_core::model::ModelSelection::from_unchecked(Some(settings.model.as_str().to_string()));
    Ok(())
}

#[derive(Clone, Default)]
struct SharedTurnOutput {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTurnOutput {
    fn bytes(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Write for SharedTurnOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HostedOperationWriter for SharedTurnOutput {
    fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
        self.flush()
    }
}

impl JsonlSurfaceOutput for SharedTurnOutput {}

impl JsonlSurfaceAdapter {
    pub fn additional_working_directories(
        &self,
        thread_id: &str,
    ) -> Option<Vec<std::path::PathBuf>> {
        self.read_session(thread_id, false, false)
            .ok()
            .map(|thread| {
                thread
                    .additional_working_directories
                    .into_iter()
                    .map(|directory| directory.path)
                    .collect()
            })
    }

    pub fn active_permission_profile(
        &self,
        thread_id: &str,
    ) -> Option<orca_core::config::ActivePermissionProfile> {
        self.read_session(thread_id, false, false)
            .ok()
            .and_then(|thread| thread.active_permission_profile)
    }

    pub fn thread(&self, thread_id: &str) -> Option<ServerThreadView> {
        if let Some(thread) = self.ephemeral_thread(thread_id) {
            let projection = thread.jsonl_read_live_projection(false, false).ok()?;
            return Some(ServerThreadView {
                cwd: projection.cwd,
                runtime_workspace_roots: projection.runtime_workspace_roots,
                active_permission_profile: projection.active_permission_profile,
                additional_working_directories: projection.additional_working_directories,
                metadata_writable_directories: Vec::new(),
                network_domain_permissions: projection.network_domain_permissions,
                mcp_registry: thread.mcp_registry(),
            });
        }
        let thread = self.read_session(thread_id, false, false).ok()?;
        Some(ServerThreadView {
            cwd: thread.cwd,
            runtime_workspace_roots: thread.runtime_workspace_roots,
            active_permission_profile: thread.active_permission_profile,
            additional_working_directories: thread.additional_working_directories,
            metadata_writable_directories: thread.metadata_writable_directories,
            network_domain_permissions: thread.network_domain_permissions,
            mcp_registry: self.mcp_registry(thread_id)?,
        })
    }

    pub fn run_turn<W: Write>(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        writer: W,
    ) -> io::Result<()> {
        self.run_turn_with_permissions(
            config,
            thread_id,
            prompt,
            PermissionProfileOverride::default(),
            writer,
        )
    }

    pub fn run_turn_with_permissions<W: Write>(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        mut writer: W,
    ) -> io::Result<()> {
        let prepared = self.prepare_turn(
            config,
            thread_id,
            prompt,
            permissions,
            &serde_json::Value::from("synchronous-jsonl-turn"),
        )?;
        let output = SharedTurnOutput::default();
        let mut operation = prepared.start_with_output(output.clone())?;
        operation.wait_terminal()?;
        writer.write_all(&output.bytes())
    }

    pub fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> Option<StoredThreadProjection> {
        self.read_session(thread_id, include_messages, include_turns)
            .ok()
    }

    pub fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> Option<StoredThreadTurnPage> {
        self.list_turns(thread_id, cursor, limit, sort_direction, items_view)
            .ok()
    }

    pub fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> Option<StoredThreadItemPage> {
        self.list_items(thread_id, turn_id, cursor, limit, sort_direction)
            .ok()
    }

    pub fn update_thread_metadata(&mut self, thread_id: &str, patch: ThreadMetadataPatch) -> bool {
        self.update_metadata(thread_id, patch).is_ok()
    }

    pub fn has_completed_turn(&self, turn_id: &str) -> bool {
        self.completed_turn_thread_id(turn_id).is_some()
    }

    pub fn completed_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        self.list_sessions(
            None,
            usize::MAX,
            ThreadListFilters::active(),
            ThreadSortKey::UpdatedAt,
            SortDirection::Desc,
            None,
        )
        .ok()?
        .data
        .into_iter()
        .find_map(|thread| {
            self.list_turns(
                &thread.thread_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .ok()?
            .data
            .into_iter()
            .any(|turn| turn.turn_id == turn_id)
            .then_some(thread.thread_id)
        })
    }

    pub(crate) fn submission_context(
        &self,
        thread_id: &str,
        permissions: &PermissionProfileOverride,
    ) -> Option<ServerThreadSubmissionContext> {
        let thread = self.read_session(thread_id, false, false).ok()?;
        Some(ServerThreadSubmissionContext {
            cwd: thread.cwd,
            runtime_workspace_roots: permissions
                .runtime_workspace_roots
                .clone()
                .unwrap_or(thread.runtime_workspace_roots),
            mcp_registry: self.mcp_registry(thread_id)?,
        })
    }

    pub(crate) fn prepare_turn(
        &self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &serde_json::Value,
    ) -> io::Result<PreparedJsonlTurn> {
        self.prepare_turn_inner(config, thread_id, prompt, permissions, rpc_id, None)
    }

    pub(crate) fn prepare_turn_with_interactions(
        &self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &serde_json::Value,
        interactions: JsonlInteractionTransport,
    ) -> io::Result<PreparedJsonlTurn> {
        self.prepare_turn_inner(
            config,
            thread_id,
            prompt,
            permissions,
            rpc_id,
            Some(interactions),
        )
    }

    pub(crate) fn register_transport_turn(&mut self, turn: JsonlTransportTurn) {
        self.transport_turns.push(turn);
    }

    pub(crate) fn prune_finished_turns(&mut self) {
        let mut pending = Vec::with_capacity(self.transport_turns.len());
        for mut turn in self.transport_turns.drain(..) {
            if turn.is_finished() {
                let _ = turn.wait_terminal();
            } else {
                pending.push(turn);
            }
        }
        self.transport_turns = pending;
    }

    pub(crate) fn wait_clean_eof_one_shots(&mut self) -> io::Result<()> {
        let mut pending = Vec::with_capacity(self.transport_turns.len());
        let mut completion_error = None;
        for mut turn in self.transport_turns.drain(..) {
            if turn.clean_eof_policy == JsonlCleanEofPolicy::CompleteEphemeralOneShot {
                if let Err(error) = turn.wait_terminal() {
                    completion_error.get_or_insert(error);
                }
            } else {
                pending.push(turn);
            }
        }
        self.transport_turns = pending;
        completion_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    pub(crate) fn wait_active_turns(&mut self) {
        for turn in &mut self.transport_turns {
            let _ = turn.wait_terminal();
        }
        self.transport_turns.clear();
    }

    pub(crate) fn control_turn(
        &self,
        thread_id: Option<&str>,
        turn_id: &str,
        action: crate::surface::JsonlTurnControlAction,
    ) -> io::Result<crate::surface::JsonlTurnControlResult> {
        self.control_turn_inner(thread_id, turn_id, action, None)
    }

    pub fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage> {
        self.list_sessions(
            cursor,
            limit,
            filters,
            sort_key,
            sort_direction,
            search_term,
        )
    }

    pub fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage> {
        self.search_sessions(
            query,
            cursor,
            limit,
            include_archived,
            sort_key,
            sort_direction,
        )
    }

    pub fn read_thread_result(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection> {
        self.read_session(thread_id, include_messages, include_turns)
    }

    pub fn list_thread_turns_result(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage> {
        self.list_turns(thread_id, cursor, limit, sort_direction, items_view)
    }

    pub fn list_thread_items_result(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage> {
        self.list_items(thread_id, turn_id, cursor, limit, sort_direction)
    }

    pub fn update_thread_metadata_result(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<()> {
        self.update_metadata(thread_id, patch)
    }
}

impl PreparedJsonlTurn {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn turn_id(&self) -> &TurnId {
        self.expected_turn_id
            .as_ref()
            .expect("recorded JSONL turns provide their legacy turn id before admission")
    }

    pub(crate) fn start<W>(self, writer: W) -> io::Result<JsonlTransportTurn>
    where
        W: JsonlSurfaceOutput + Send + 'static,
    {
        committed(
            self.client.admit_reserved_with_output(
                SurfaceRequestId::new(),
                self.operation_id.clone(),
                self.admission_lease_id,
                DiscardHostedOperationWriter,
            ),
            "JSONL surface admission",
        )?;
        let mut subscription = self.subscription;
        let (turn_id, prefetched) =
            receive_admitted_turn(&mut subscription, &self.operation_id, self.expected_turn_id)?;
        let thread_id = self.thread_id.clone();
        let transport_turn_id = turn_id.clone();
        let clean_eof_policy = self.clean_eof_policy;
        let ephemeral_thread = self.ephemeral_thread;
        let worker = std::thread::spawn(move || {
            let _ephemeral_thread = ephemeral_thread;
            drain_jsonl_surface(
                self.surface,
                self.client,
                subscription,
                prefetched,
                self.operation_id,
                self.thread_id,
                turn_id,
                self.interactions,
                self.runtime_workspace_roots,
                writer,
            )
        });
        Ok(JsonlTransportTurn {
            thread_id,
            turn_id: transport_turn_id,
            clean_eof_policy,
            worker: Some(worker),
        })
    }

    pub(crate) fn start_with_output<W>(self, writer: W) -> io::Result<JsonlTransportTurn>
    where
        W: JsonlSurfaceOutput + Send + 'static,
    {
        self.start(writer)
    }
}

fn receive_admitted_turn(
    subscription: &mut SurfaceSubscriptionReceiver,
    operation_id: &SurfaceOperationId,
    expected_turn_id: Option<TurnId>,
) -> io::Result<(TurnId, VecDeque<SurfaceSubscriptionItem>)> {
    let mut prefetched = VecDeque::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let Some(item) = subscription.recv_timeout(std::time::Duration::from_millis(100)) else {
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::other(
                    "JSONL surface admission event unavailable",
                ));
            }
            continue;
        };
        let admitted_turn_id =
            match &item {
                SurfaceSubscriptionItem::Batch { batch } => batch
                    .events
                    .as_slice()
                    .iter()
                    .find_map(|envelope| match &envelope.event {
                        SurfaceEvent::Operation(OperationPatch::Admitted {
                            operation_id: admitted_operation_id,
                            logical_turn_id,
                            ..
                        }) if admitted_operation_id == operation_id => {
                            Some(logical_turn_id.clone())
                        }
                        _ => None,
                    }),
                SurfaceSubscriptionItem::Gap { .. } => {
                    return Err(io::Error::other(
                        "JSONL surface snapshot required before admission was observed",
                    ));
                }
                SurfaceSubscriptionItem::Sealed { .. } => {
                    return Err(io::Error::other(
                        "JSONL surface sealed before admission was observed",
                    ));
                }
            };
        prefetched.push_back(item);
        if let Some(turn_id) = admitted_turn_id {
            if expected_turn_id
                .as_ref()
                .is_some_and(|expected| expected != &turn_id)
            {
                return Err(io::Error::other(
                    "JSONL surface admitted a different logical turn id",
                ));
            }
            return Ok((turn_id, prefetched));
        }
    }
}

impl JsonlTransportTurn {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    pub(crate) fn wait_terminal(&mut self) -> io::Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| io::Error::other("JSONL surface projection worker panicked"))?
    }
}

#[derive(Default)]
struct DiscardHostedOperationWriter;

impl Write for DiscardHostedOperationWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HostedOperationWriter for DiscardHostedOperationWriter {
    fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
        Ok(())
    }
}

fn drain_jsonl_surface<W>(
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    mut subscription: SurfaceSubscriptionReceiver,
    mut prefetched: VecDeque<SurfaceSubscriptionItem>,
    operation_id: SurfaceOperationId,
    thread_id: String,
    turn_id: TurnId,
    interactions: Option<JsonlInteractionTransport>,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    mut writer: W,
) -> io::Result<()>
where
    W: JsonlSurfaceOutput,
{
    let _detach = SurfaceDetachGuard::new(surface.clone(), client.clone());
    let mut projector = JsonlSurfaceProjector::new(
        surface.clone(),
        thread_id,
        turn_id,
        operation_id,
        client.clone(),
        interactions,
        runtime_workspace_roots,
    );
    let result = loop {
        let item = if let Some(item) = prefetched.pop_front() {
            item
        } else {
            let Some(item) = subscription.recv_timeout(std::time::Duration::from_millis(100))
            else {
                continue;
            };
            item
        };
        match item {
            SurfaceSubscriptionItem::Batch { batch } => {
                if project_surface_batch(&mut projector, &batch, &mut writer)? {
                    writer.finish_generation(true)?;
                    break Ok(());
                }
            }
            SurfaceSubscriptionItem::Gap { .. } => {
                write_runtime_event(
                    &mut writer,
                    "error",
                    &projector.thread_id,
                    serde_json::json!({
                        "message": "thread surface snapshot required; reconnect and resume the thread"
                    }),
                )?;
                writer.finish_generation(false)?;
                break Err(io::Error::other(
                    "thread surface snapshot required; reconnect and resume the thread",
                ));
            }
            SurfaceSubscriptionItem::Sealed { .. } => {
                writer.finish_generation(false)?;
                break Err(io::Error::other("JSONL surface subscription sealed"));
            }
        }
    };
    result
}

struct SurfaceDetachGuard {
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
}

impl SurfaceDetachGuard {
    fn new(surface: RuntimeSurfaceHandle, client: RuntimeSurfaceClientHandle) -> Self {
        Self { surface, client }
    }
}

impl Drop for SurfaceDetachGuard {
    fn drop(&mut self) {
        let _ = self.surface.detach(
            &self.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
    }
}

struct JsonlSurfaceProjector {
    surface: RuntimeSurfaceHandle,
    thread_id: String,
    turn_id: TurnId,
    operation_id: SurfaceOperationId,
    streams: HashMap<String, SurfaceAssistantStream>,
    started_assistant_items: HashSet<String>,
    tools: HashMap<String, SurfaceToolRequest>,
    workflows: HashMap<String, JsonlProjectedWorkflow>,
    client: RuntimeSurfaceClientHandle,
    interactions: Option<JsonlInteractionTransport>,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
}

impl JsonlSurfaceProjector {
    fn new(
        surface: RuntimeSurfaceHandle,
        thread_id: String,
        turn_id: TurnId,
        operation_id: SurfaceOperationId,
        client: RuntimeSurfaceClientHandle,
        interactions: Option<JsonlInteractionTransport>,
        runtime_workspace_roots: Vec<std::path::PathBuf>,
    ) -> Self {
        Self {
            surface,
            thread_id,
            turn_id,
            operation_id,
            streams: HashMap::new(),
            started_assistant_items: HashSet::new(),
            tools: HashMap::new(),
            workflows: HashMap::new(),
            client,
            interactions,
            runtime_workspace_roots,
        }
    }
}

#[derive(Clone)]
struct JsonlProjectedWorkflow {
    task_id: String,
    run_id: String,
    workflow_name: String,
    tool_call_id: Option<String>,
    terminal_status: Option<SurfaceWorkflowResultStatus>,
}

fn project_surface_batch<W: JsonlSurfaceOutput>(
    projector: &mut JsonlSurfaceProjector,
    batch: &SurfaceCommitBatch,
    writer: &mut W,
) -> io::Result<bool> {
    for envelope in batch.events.as_slice() {
        if !surface_event_belongs_to_operation(
            &envelope.scope,
            &envelope.event,
            &projector.operation_id,
        ) {
            continue;
        }
        if project_surface_event(projector, &envelope.event, writer)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn surface_event_belongs_to_operation(
    scope: &SurfaceScope,
    event: &SurfaceEvent,
    operation_id: &SurfaceOperationId,
) -> bool {
    match scope {
        SurfaceScope::Thread => match event {
            SurfaceEvent::Plan(plan) => plan
                .causative_generation
                .as_ref()
                .is_some_and(|fence| &fence.operation_id == operation_id),
            SurfaceEvent::Workflow(WorkflowPatch::Started { workflow }) => workflow
                .parent
                .as_ref()
                .is_some_and(|fence| &fence.operation_id == operation_id),
            SurfaceEvent::Workflow(
                WorkflowPatch::Resumed { fence, .. }
                | WorkflowPatch::PhaseStarted { fence, .. }
                | WorkflowPatch::PhaseCompleted { fence, .. }
                | WorkflowPatch::AgentStarted { fence, .. }
                | WorkflowPatch::AgentCached { fence, .. }
                | WorkflowPatch::AgentCompleted { fence, .. }
                | WorkflowPatch::AgentFailed { fence, .. }
                | WorkflowPatch::AgentCancelled { fence, .. }
                | WorkflowPatch::Paused { fence, .. }
                | WorkflowPatch::Stopping { fence, .. }
                | WorkflowPatch::Stopped { fence, .. }
                | WorkflowPatch::AsyncLaunched { fence, .. }
                | WorkflowPatch::Completed { fence, .. }
                | WorkflowPatch::Failed { fence, .. }
                | WorkflowPatch::Cancelled { fence, .. }
                | WorkflowPatch::ResultReady { fence, .. },
            ) => fence
                .parent
                .as_ref()
                .is_some_and(|parent| &parent.operation_id == operation_id),
            _ => false,
        },
        _ => scope_belongs_to_operation(scope, operation_id),
    }
}

fn scope_belongs_to_operation(scope: &SurfaceScope, operation_id: &SurfaceOperationId) -> bool {
    match scope {
        SurfaceScope::Thread => false,
        SurfaceScope::Operation {
            operation_id: scoped,
        } => scoped == operation_id,
        SurfaceScope::Generation { fence } => &fence.operation_id == operation_id,
        SurfaceScope::Background { fence } => &fence.operation_fence.operation_id == operation_id,
        SurfaceScope::Goal {
            causative_generation,
            ..
        } => causative_generation
            .as_ref()
            .is_some_and(|fence| &fence.operation_id == operation_id),
    }
}

fn ensure_assistant_item_started<W: JsonlSurfaceOutput>(
    projector: &mut JsonlSurfaceProjector,
    writer: &mut W,
    item_id: String,
    item: serde_json::Value,
) -> io::Result<()> {
    if projector.started_assistant_items.insert(item_id) {
        writer.write_server_event(ServerEvent::ItemStarted {
            thread_id: serde_json::Value::from(projector.thread_id.clone()),
            turn_id: serde_json::Value::from(projector.turn_id.to_string()),
            item,
        })?;
    }
    Ok(())
}

fn project_surface_event<W: JsonlSurfaceOutput>(
    projector: &mut JsonlSurfaceProjector,
    event: &SurfaceEvent,
    writer: &mut W,
) -> io::Result<bool> {
    match event {
        SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted { turn }) => {
            let legacy_turn_number = turn.ordinal.saturating_add(1);
            let legacy_task_id = format!("{}:task-{legacy_turn_number}", projector.thread_id);
            write_runtime_event(
                writer,
                "turn.started",
                &projector.thread_id,
                serde_json::json!({
                    "turn_id": projector.turn_id.to_string(),
                    "turn": legacy_turn_number,
                    "task": {
                        "task_id": legacy_task_id,
                        "kind": "agent",
                        "status": "running",
                        "turn": legacy_turn_number,
                    },
                }),
            )?;
        }
        SurfaceEvent::Operation(OperationPatch::Terminal { record })
            if record.operation_id == projector.operation_id =>
        {
            let _ = projector.surface.detach(
                &projector.client,
                DetachRequest {
                    request_id: SurfaceRequestId::new(),
                },
            );
            if let OperationTerminal::Failed { message, .. } = &record.terminal {
                write_runtime_event(
                    writer,
                    "error",
                    &projector.thread_id,
                    serde_json::json!({ "message": message.as_str() }),
                )?;
            }
            write_runtime_event(
                writer,
                "session.completed",
                &projector.thread_id,
                serde_json::json!({ "status": terminal_status(&record.terminal) }),
            )?;
            return Ok(true);
        }
        SurfaceEvent::Assistant(AssistantPatch::StreamOpened { stream }) => {
            projector
                .streams
                .insert(serialized_id(&stream.stream_id), stream.clone());
        }
        SurfaceEvent::Assistant(AssistantPatch::Delta {
            stream_id, text, ..
        }) => {
            let Some(stream) = projector.streams.get(&serialized_id(stream_id)).cloned() else {
                return Ok(false);
            };
            if !writer.supports_direct_server_events() {
                let event_type = match stream.channel {
                    AssistantChannel::Message => "assistant.message.delta",
                    AssistantChannel::Reasoning => "assistant.reasoning.delta",
                    AssistantChannel::Plan => "assistant.message.delta",
                };
                let text = match stream.channel {
                    AssistantChannel::Plan => {
                        format!("<proposed_plan>{}</proposed_plan>", text.as_str())
                    }
                    _ => text.as_str().to_string(),
                };
                let payload = match stream.channel {
                    AssistantChannel::Reasoning => serde_json::json!({
                        "turn_id": projector.turn_id.to_string(),
                        "item_id": stream.item_id.to_string(),
                        "text": text,
                    }),
                    _ => serde_json::json!({
                        "turn_id": projector.turn_id.to_string(),
                        "agent_message_item_id": stream.item_id.to_string(),
                        "plan_item_id": stream.item_id.to_string(),
                        "text": text,
                    }),
                };
                write_runtime_event(writer, event_type, &projector.thread_id, payload)?;
                return Ok(false);
            }
            match stream.channel {
                AssistantChannel::Message => {
                    // Message streams can contain provider-local proposed-plan markup. Wait for
                    // the typed completed response so plan and message items retain their IDs.
                }
                AssistantChannel::Reasoning => {
                    let item_id = stream.item_id.to_string();
                    ensure_assistant_item_started(
                        projector,
                        writer,
                        item_id.clone(),
                        serde_json::json!({
                            "type": "reasoning",
                            "id": item_id,
                            "summary": "",
                            "content": "",
                        }),
                    )?;
                    writer.write_server_event(ServerEvent::ItemReasoningDelta {
                        item_id: serde_json::Value::from(stream.item_id.to_string()),
                        delta: serde_json::Value::from(text.as_str().to_string()),
                    })?;
                    writer.write_server_event(ServerEvent::ReasoningDelta {
                        text: serde_json::Value::from(text.as_str().to_string()),
                    })?;
                }
                AssistantChannel::Plan => {
                    let item_id = stream.item_id.to_string();
                    ensure_assistant_item_started(
                        projector,
                        writer,
                        item_id.clone(),
                        serde_json::json!({ "type": "plan", "id": item_id, "text": "" }),
                    )?;
                    writer.write_server_event(ServerEvent::ItemPlanDelta {
                        item_id: serde_json::Value::from(stream.item_id.to_string()),
                        delta: serde_json::Value::from(text.as_str().to_string()),
                    })?;
                }
            }
        }
        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response }) => {
            if !writer.supports_direct_server_events() {
                let message_id = response
                    .message_item
                    .as_ref()
                    .map(|item| item.id.clone())
                    .unwrap_or_else(orca_core::thread_identity::ConversationItemId::new);
                let plan_id = response
                    .plan_item
                    .as_ref()
                    .map(|item| item.id.clone())
                    .unwrap_or_else(orca_core::thread_identity::ConversationItemId::new);
                let reasoning_id = response
                    .reasoning_item
                    .as_ref()
                    .map(|item| item.id.clone())
                    .unwrap_or_else(orca_core::thread_identity::ConversationItemId::new);
                let mut assistant_content = response
                    .message_item
                    .as_ref()
                    .map(|item| item.text.as_str().to_string())
                    .unwrap_or_default();
                if let Some(plan) = response.plan_item.as_ref() {
                    assistant_content.push_str("<proposed_plan>");
                    assistant_content.push_str(plan.text.as_str());
                    assistant_content.push_str("</proposed_plan>");
                }
                let assistant_reasoning = response.reasoning_item.as_ref().map(|item| {
                    if item.content.as_str().is_empty() {
                        item.summary.as_str().to_string()
                    } else {
                        item.content.as_str().to_string()
                    }
                });
                write_runtime_event(
                    writer,
                    "model.response.completed",
                    &projector.thread_id,
                    serde_json::json!({
                        "identity": {
                            "turn_id": projector.turn_id.to_string(),
                            "item_ids": {
                                "conversation_item_id": message_id.to_string(),
                                "plan_item_id": plan_id.to_string(),
                                "reasoning_item_id": reasoning_id.to_string(),
                            },
                        },
                        "assistant_content": (!assistant_content.is_empty()).then_some(assistant_content),
                        "assistant_reasoning": assistant_reasoning,
                        "tool_calls": [],
                    }),
                )?;
                return Ok(false);
            }
            if let Some(message) = response.message_item.as_ref() {
                let item_id = message.id.to_string();
                ensure_assistant_item_started(
                    projector,
                    writer,
                    item_id.clone(),
                    serde_json::json!({ "type": "agent_message", "id": item_id, "text": "" }),
                )?;
                if !message.text.as_str().is_empty() {
                    writer.write_server_event(ServerEvent::ItemMessageDelta {
                        item_id: serde_json::Value::from(message.id.to_string()),
                        delta: serde_json::Value::from(message.text.as_str().to_string()),
                    })?;
                    writer.write_server_event(ServerEvent::MessageDelta {
                        text: serde_json::Value::from(message.text.as_str().to_string()),
                    })?;
                }
                writer.write_server_event(ServerEvent::ItemCompleted {
                    thread_id: serde_json::Value::from(projector.thread_id.clone()),
                    turn_id: serde_json::Value::from(projector.turn_id.to_string()),
                    item: serde_json::json!({
                        "type": "agent_message",
                        "id": message.id.to_string(),
                        "text": message.text.as_str(),
                    }),
                })?;
            }
            if let Some(plan) = response.plan_item.as_ref() {
                let item_id = plan.id.to_string();
                ensure_assistant_item_started(
                    projector,
                    writer,
                    item_id.clone(),
                    serde_json::json!({ "type": "plan", "id": item_id, "text": "" }),
                )?;
                if !plan.text.as_str().is_empty() {
                    writer.write_server_event(ServerEvent::ItemPlanDelta {
                        item_id: serde_json::Value::from(plan.id.to_string()),
                        delta: serde_json::Value::from(plan.text.as_str().to_string()),
                    })?;
                }
                writer.write_server_event(ServerEvent::ItemCompleted {
                    thread_id: serde_json::Value::from(projector.thread_id.clone()),
                    turn_id: serde_json::Value::from(projector.turn_id.to_string()),
                    item: serde_json::json!({
                        "type": "plan",
                        "id": plan.id.to_string(),
                        "text": plan.text.as_str(),
                    }),
                })?;
            }
            if let Some(reasoning) = response.reasoning_item.as_ref() {
                let item_id = reasoning.id.to_string();
                let (summary, content) = if reasoning.summary.as_str().is_empty()
                    && !reasoning.content.as_str().is_empty()
                {
                    (reasoning.content.as_str(), "")
                } else {
                    (reasoning.summary.as_str(), reasoning.content.as_str())
                };
                ensure_assistant_item_started(
                    projector,
                    writer,
                    item_id.clone(),
                    serde_json::json!({
                        "type": "reasoning",
                        "id": item_id,
                        "summary": "",
                        "content": "",
                    }),
                )?;
                writer.write_server_event(ServerEvent::ItemCompleted {
                    thread_id: serde_json::Value::from(projector.thread_id.clone()),
                    turn_id: serde_json::Value::from(projector.turn_id.to_string()),
                    item: serde_json::json!({
                        "type": "reasoning",
                        "id": reasoning.id.to_string(),
                        "summary": summary,
                        "content": content,
                    }),
                })?;
            }
        }
        SurfaceEvent::Assistant(AssistantPatch::StreamDiscarded { stream_id, .. }) => {
            projector.streams.remove(&serialized_id(stream_id));
        }
        SurfaceEvent::Tool(ToolPatch::Requested { request }) => {
            projector
                .tools
                .insert(request.tool_call_id.as_str().to_string(), request.clone());
            write_runtime_event(
                writer,
                "tool.call.requested",
                &projector.thread_id,
                serde_json::json!({
                    "id": request.tool_call_id.as_str(),
                    "name": request.name.as_str(),
                    "target": request.target.as_ref().map(DisplayText::as_str),
                    "raw_arguments": request.raw_arguments.as_str(),
                }),
            )?;
        }
        SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
            let request = projector.tools.remove(result.tool_call_id.as_str());
            write_runtime_event(
                writer,
                "tool.call.completed",
                &projector.thread_id,
                serde_json::json!({
                    "id": result.tool_call_id.as_str(),
                    "name": result.name.as_str(),
                    "target": request.as_ref().and_then(|value| value.target.as_ref()).map(DisplayText::as_str),
                    "raw_arguments": request.as_ref().map(|value| value.raw_arguments.as_str()),
                    "status": tool_status(result.terminal.kind),
                    "output": result.output.as_ref().map(DisplayText::as_str),
                    "error": result.error.as_ref().map(DisplayText::as_str),
                    "exit_code": result.exit_code,
                    "kind": tool_result_kind(result.terminal.kind),
                    "terminal_source": match result.terminal.source {
                        ToolTerminalSource::Observed => "observed",
                        ToolTerminalSource::CompatibilityRepair => "compatibility_repair",
                    },
                    "truncated": result.truncated,
                }),
            )?;
        }
        SurfaceEvent::Workflow(WorkflowPatch::Started { workflow }) => {
            let run_id = workflow.workflow_run_id.as_str().to_string();
            let projected = JsonlProjectedWorkflow {
                task_id: workflow.task_id.as_str().to_string(),
                run_id: run_id.clone(),
                workflow_name: workflow.name.as_str().to_string(),
                tool_call_id: None,
                terminal_status: None,
            };
            write_runtime_event(
                writer,
                "workflow.started",
                &projector.thread_id,
                serde_json::json!({
                    "taskId": projected.task_id,
                    "runId": projected.run_id,
                    "workflowName": projected.workflow_name,
                    "phases": workflow.phases.iter().map(|phase| phase.name.as_str()).collect::<Vec<_>>(),
                    "task": jsonl_workflow_task(&projected.run_id, "running"),
                }),
            )?;
            projector.workflows.insert(run_id, projected);
        }
        SurfaceEvent::Workflow(WorkflowPatch::Completed { fence, .. }) => {
            let run_id = fence.workflow_run_id.as_str();
            let Some(workflow) = projector.workflows.get_mut(run_id) else {
                return Ok(false);
            };
            workflow.terminal_status = Some(SurfaceWorkflowResultStatus::Success);
            let workflow = workflow.clone();
            write_runtime_event(
                writer,
                "workflow.completed",
                &projector.thread_id,
                serde_json::json!({
                    "taskId": workflow.task_id,
                    "runId": workflow.run_id,
                    "workflowName": workflow.workflow_name,
                    "task": jsonl_workflow_task(&workflow.run_id, "succeeded"),
                }),
            )?;
        }
        SurfaceEvent::Workflow(WorkflowPatch::Failed { fence, error, .. }) => {
            let run_id = fence.workflow_run_id.as_str();
            let Some(workflow) = projector.workflows.get_mut(run_id) else {
                return Ok(false);
            };
            workflow.terminal_status = Some(SurfaceWorkflowResultStatus::Failed);
            let workflow = workflow.clone();
            write_runtime_event(
                writer,
                "workflow.failed",
                &projector.thread_id,
                serde_json::json!({
                    "taskId": workflow.task_id,
                    "runId": workflow.run_id,
                    "workflowName": workflow.workflow_name,
                    "toolUseId": workflow.tool_call_id,
                    "status": "failed",
                    "error": error.as_str(),
                    "task": jsonl_workflow_task(&workflow.run_id, "failed"),
                }),
            )?;
        }
        SurfaceEvent::Workflow(WorkflowPatch::Cancelled { fence, reason, .. }) => {
            let run_id = fence.workflow_run_id.as_str();
            let Some(workflow) = projector.workflows.get_mut(run_id) else {
                return Ok(false);
            };
            workflow.terminal_status = Some(SurfaceWorkflowResultStatus::Failed);
            let workflow = workflow.clone();
            write_runtime_event(
                writer,
                "workflow.failed",
                &projector.thread_id,
                serde_json::json!({
                    "taskId": workflow.task_id,
                    "runId": workflow.run_id,
                    "workflowName": workflow.workflow_name,
                    "toolUseId": workflow.tool_call_id,
                    "status": "failed",
                    "error": reason.as_str(),
                    "task": jsonl_workflow_task(&workflow.run_id, "cancelled"),
                }),
            )?;
        }
        SurfaceEvent::Workflow(WorkflowPatch::ResultReady { fence, result, .. }) => {
            let run_id = fence.workflow_run_id.as_str();
            let Some(mut workflow) = projector.workflows.remove(run_id) else {
                return Ok(false);
            };
            workflow.tool_call_id = result
                .tool_use_id
                .as_ref()
                .map(|tool_call_id| tool_call_id.as_str().to_string());
            if result.status == SurfaceWorkflowResultStatus::Success
                && workflow.terminal_status == Some(SurfaceWorkflowResultStatus::Success)
            {
                write_runtime_event(
                    writer,
                    "workflow.result.available",
                    &projector.thread_id,
                    serde_json::json!({
                        "taskId": workflow.task_id,
                        "runId": workflow.run_id,
                        "workflowName": workflow.workflow_name,
                        "toolUseId": workflow.tool_call_id,
                        "status": "completed",
                        "result": result.content.as_str(),
                        "task": jsonl_workflow_task(&workflow.run_id, "succeeded"),
                    }),
                )?;
            }
        }
        SurfaceEvent::Plan(plan) => {
            let items = plan
                .items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "step": item.step.as_str(),
                        "status": match item.status {
                            crate::surface::SurfacePlanStatus::Pending => "pending",
                            crate::surface::SurfacePlanStatus::InProgress => "in_progress",
                            crate::surface::SurfacePlanStatus::Completed => "completed",
                        },
                    })
                })
                .collect::<Vec<_>>();
            if writer.supports_direct_server_events() {
                writer.write_server_event(ServerEvent::TurnPlanUpdated {
                    thread_id: serde_json::Value::Null,
                    turn_id: serde_json::Value::Null,
                    explanation: serde_json::to_value(
                        plan.explanation.as_ref().map(DisplayText::as_str),
                    )?,
                    plan: serde_json::Value::Array(items),
                })?;
            } else {
                write_runtime_event(
                    writer,
                    "plan.updated",
                    &projector.thread_id,
                    serde_json::json!({
                        "explanation": plan.explanation.as_ref().map(DisplayText::as_str),
                        "plan": items,
                    }),
                )?;
            }
        }
        SurfaceEvent::Interaction(InteractionPatch::Requested { interaction })
            if routes_interaction(&projector.client, &interaction.route) =>
        {
            let Some(transport) = projector.interactions.as_ref() else {
                return Ok(false);
            };
            let request_id = format!(
                "{}-{}",
                projector.turn_id,
                serialized_id(&interaction.interaction_id)
            );
            match &interaction.request {
                SurfaceInteractionRequest::ToolApproval { description, .. } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.permissions.register(
                            request_id.clone(),
                            JsonlRetiredRequestOwner::ThreadPermission,
                            JsonlPermissionRoute::Surface {
                                client: projector.client.clone(),
                                interaction_id: interaction.interaction_id.clone(),
                                target: interaction.kind,
                                thread_id: projector.thread_id.clone(),
                                runtime_workspace_roots: projector.runtime_workspace_roots.clone(),
                            },
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "reason": description.as_str(),
                        "permissions": {},
                    });
                    let frame_digest =
                        super::opaque_permission_router::jsonl_response_digest(&payload)?;
                    transport
                        .permissions
                        .mark_writing(&request_id, frame_digest)?;
                    write_runtime_event(
                        writer,
                        "surface.permission.requested",
                        &projector.thread_id,
                        payload,
                    )?;
                    writer.flush()?;
                    transport
                        .permissions
                        .mark_published(&request_id, frame_digest)?;
                }
                SurfaceInteractionRequest::PermissionRequest {
                    context,
                    reason,
                    permissions,
                    ..
                } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.permissions.register(
                            request_id.clone(),
                            JsonlRetiredRequestOwner::ThreadPermission,
                            JsonlPermissionRoute::Surface {
                                client: projector.client.clone(),
                                interaction_id: interaction.interaction_id.clone(),
                                target: interaction.kind,
                                thread_id: projector.thread_id.clone(),
                                runtime_workspace_roots: projector.runtime_workspace_roots.clone(),
                            },
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "reason": reason.as_ref().map(DisplayText::as_str),
                        "permissions": surface_permissions_wire(permissions),
                        "context": context,
                    });
                    let frame_digest =
                        super::opaque_permission_router::jsonl_response_digest(&payload)?;
                    transport
                        .permissions
                        .mark_writing(&request_id, frame_digest)?;
                    write_runtime_event(
                        writer,
                        "surface.permission.requested",
                        &projector.thread_id,
                        payload,
                    )?;
                    writer.flush()?;
                    transport
                        .permissions
                        .mark_published(&request_id, frame_digest)?;
                }
                SurfaceInteractionRequest::UserInput {
                    question,
                    suggestions,
                } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.direct.register(
                            request_id.clone(),
                            JsonlDirectInteractionKind::UserInput,
                            JsonlDirectInteractionRoute::UserInput {
                                client: projector.client.clone(),
                                interaction_id: interaction.interaction_id.clone(),
                            },
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "question": question.as_str(),
                        "choices": suggestions.iter().map(DisplayText::as_str).collect::<Vec<_>>(),
                    });
                    transport.direct.publish(&request_id, || {
                        write_runtime_event(
                            writer,
                            "surface.user_input.requested",
                            &projector.thread_id,
                            payload,
                        )?;
                        writer.flush()
                    })?;
                }
                SurfaceInteractionRequest::McpElicitation {
                    server_name,
                    message,
                    request,
                    ..
                } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.direct.register(
                            request_id.clone(),
                            JsonlDirectInteractionKind::McpElicitation,
                            JsonlDirectInteractionRoute::McpElicitation {
                                client: projector.client.clone(),
                                interaction_id: interaction.interaction_id.clone(),
                            },
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let (mode, url, requested_schema) = match request {
                        crate::surface::SurfaceMcpElicitationRequest::Form {
                            requested_schema,
                            ..
                        } => (
                            "form",
                            serde_json::Value::Null,
                            requested_schema
                                .as_ref()
                                .map(surface_data_wire)
                                .unwrap_or(serde_json::Value::Null),
                        ),
                        crate::surface::SurfaceMcpElicitationRequest::Url {
                            raw_url,
                            requested_schema,
                        } => (
                            "url",
                            raw_url
                                .as_ref()
                                .map(|url| serde_json::Value::from(url.as_str()))
                                .unwrap_or(serde_json::Value::Null),
                            requested_schema
                                .as_ref()
                                .map(surface_data_wire)
                                .unwrap_or(serde_json::Value::Null),
                        ),
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "server_name": server_name.as_str(),
                        "mode": mode,
                        "message": message.as_str(),
                        "url": url,
                        "requested_schema": requested_schema,
                    });
                    transport.direct.publish(&request_id, || {
                        write_runtime_event(
                            writer,
                            "surface.mcp_elicitation.requested",
                            &projector.thread_id,
                            payload,
                        )?;
                        writer.flush()
                    })?;
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(false)
}

fn register_or_settle_unavailable(
    registration: io::Result<String>,
    projector: &JsonlSurfaceProjector,
    interaction: &crate::surface::SurfaceInteractionView,
) -> io::Result<Option<String>> {
    let Err(registration_error) = registration else {
        return Ok(registration.ok());
    };
    let answer = match &interaction.request {
        SurfaceInteractionRequest::ToolApproval { .. } => {
            crate::surface::SurfaceClientInteractionAnswer::ToolApproval {
                decision: crate::surface::SurfaceAllowDeny::Deny,
            }
        }
        SurfaceInteractionRequest::PermissionRequest { permissions, .. } => {
            crate::surface::SurfaceClientInteractionAnswer::PermissionRequest {
                decision: crate::surface::SurfacePermissionClientDecision::Deny {
                    scope: crate::surface::PermissionGrantScope::Turn,
                    permissions: permissions.clone(),
                    strict_auto_review: false,
                },
            }
        }
        SurfaceInteractionRequest::UserInput { .. } => {
            crate::surface::SurfaceClientInteractionAnswer::UserInput {
                decision: crate::surface::SurfaceUserInputDecision::Cancel,
            }
        }
        SurfaceInteractionRequest::McpElicitation { .. } => {
            crate::surface::SurfaceClientInteractionAnswer::McpElicitation {
                decision: crate::surface::SurfaceMcpElicitationDecision::Decline,
            }
        }
        SurfaceInteractionRequest::BackgroundApproval { .. } => {
            return Err(io::Error::other(format!(
                "{registration_error}; JSONL background approval routing is not active"
            )));
        }
    };
    match projector.client.respond_interaction_by_id(
        SurfaceRequestId::new(),
        interaction.interaction_id.clone(),
        answer,
    ) {
        Ok(MutationReply::Committed { .. }) | Ok(MutationReply::Deferred { .. }) => Ok(None),
        Ok(MutationReply::Uncommitted { .. }) | Err(_) => Err(io::Error::other(format!(
            "{registration_error}; runtime retained recovery ownership for rejected JSONL interaction"
        ))),
    }
}

fn surface_data_wire(value: &crate::surface::SurfaceDataValue) -> serde_json::Value {
    match value {
        crate::surface::SurfaceDataValue::Null => serde_json::Value::Null,
        crate::surface::SurfaceDataValue::Boolean(value) => serde_json::Value::Bool(*value),
        crate::surface::SurfaceDataValue::Integer(value) => serde_json::Value::from(value.get()),
        crate::surface::SurfaceDataValue::Unsigned(value) => serde_json::Value::from(*value),
        crate::surface::SurfaceDataValue::Number(value) => {
            serde_json::json!(value.get())
        }
        crate::surface::SurfaceDataValue::String(value) => serde_json::Value::from(value.as_str()),
        crate::surface::SurfaceDataValue::Array(values) => {
            serde_json::Value::Array(values.iter().map(surface_data_wire).collect())
        }
        crate::surface::SurfaceDataValue::Object(properties) => serde_json::Value::Object(
            properties
                .iter()
                .map(|property| {
                    (
                        property.name.as_str().to_string(),
                        surface_data_wire(&property.value),
                    )
                })
                .collect(),
        ),
    }
}

fn routes_interaction(
    client: &RuntimeSurfaceClientHandle,
    route: &SurfaceInteractionRoute,
) -> bool {
    match route {
        SurfaceInteractionRoute::Unassigned { .. } => false,
        SurfaceInteractionRoute::Exclusive { attachment_id, .. } => {
            attachment_id == client.attachment_id()
        }
        SurfaceInteractionRoute::SharedFirstCommitWins { attachments, .. } => {
            attachments.as_set().contains(client.attachment_id())
        }
    }
}

fn surface_permissions_wire(
    permissions: &crate::surface::SurfacePermissionProfile,
) -> serde_json::Value {
    let file_system = permissions.file_system.as_ref().map(|profile| {
        serde_json::json!({
            "read": profile.read.as_ref().map(|paths| paths.iter().map(|path| path.0.as_str()).collect::<Vec<_>>()),
            "write": profile.write.as_ref().map(|paths| paths.iter().map(|path| path.0.as_str()).collect::<Vec<_>>()),
        })
    });
    let network = permissions.network.as_ref().map(|profile| {
        let domains = profile
            .domains
            .iter()
            .map(|(domain, access)| {
                (
                    domain.0.as_str().to_string(),
                    serde_json::Value::from(match access {
                        crate::surface::SurfaceAllowDeny::Allow => "allow",
                        crate::surface::SurfaceAllowDeny::Deny => "deny",
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "enabled": profile.enabled,
            "domains": domains,
        })
    });
    serde_json::json!({
        "fileSystem": file_system,
        "network": network,
    })
}

fn write_runtime_event<W: Write>(
    writer: &mut W,
    event_type: &str,
    run_id: &str,
    payload: serde_json::Value,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &serde_json::json!({
            "type": event_type,
            "run_id": run_id,
            "payload": payload,
        }),
    )?;
    writeln!(writer)
}

fn serialized_id<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_default()
}

fn jsonl_workflow_task(run_id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "task_id": format!("{run_id}:task-1"),
        "kind": "workflow",
        "status": status,
        "turn": 0,
    })
}

fn terminal_status(terminal: &OperationTerminal) -> &'static str {
    match terminal {
        OperationTerminal::Succeeded { .. } => "success",
        OperationTerminal::Cancelled { .. } | OperationTerminal::Shutdown { .. } => "cancelled",
        OperationTerminal::BudgetExhausted { .. } => "budget_exhausted",
        OperationTerminal::NotAdmitted { .. } => "not_admitted",
        OperationTerminal::Failed {
            class: crate::surface::FailureClass::LegacyApprovalRequired,
            ..
        } => "approval_required",
        OperationTerminal::Failed { .. }
        | OperationTerminal::Panicked { .. }
        | OperationTerminal::JoinFailed { .. }
        | OperationTerminal::AbortedByRuntimeRestart { .. } => "failed",
    }
}

fn tool_status(kind: SurfaceToolResultKind) -> &'static str {
    match kind {
        SurfaceToolResultKind::Success => "completed",
        SurfaceToolResultKind::Cancelled => "cancelled",
        _ => "failed",
    }
}

fn tool_result_kind(kind: SurfaceToolResultKind) -> &'static str {
    match kind {
        SurfaceToolResultKind::Success => "success",
        SurfaceToolResultKind::Failed => "failed",
        SurfaceToolResultKind::Denied => "denied",
        SurfaceToolResultKind::Cancelled => "cancelled",
        SurfaceToolResultKind::TimedOut => "timed_out",
        SurfaceToolResultKind::InvalidArguments => "invalid_arguments",
        SurfaceToolResultKind::ExternalEffectAmbiguous => "external_effect_ambiguous",
        SurfaceToolResultKind::ObservationUnavailable => "observation_unavailable",
        SurfaceToolResultKind::CleanupAmbiguous => "cleanup_ambiguous",
    }
}

fn settings_preparation(
    config: &RunConfig,
    permissions: PermissionProfileOverride,
    snapshot: &crate::surface::SurfaceSnapshot,
) -> io::Result<OperationSettingsPreparation> {
    let mut updated = config.clone();
    apply_surface_settings_to_run_config(&mut updated, &snapshot.settings.effective)?;
    apply_permission_override(&mut updated, permissions);
    let patches = settings_patches(&updated, snapshot)?;
    if patches.is_empty() {
        Ok(OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: snapshot.settings.thread_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
        })
    } else {
        Ok(
            OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
                expected_settings_revision: snapshot.settings.thread_revision,
                expected_policy_epoch: snapshot.settings.effective.policy_epoch,
                patches: NonEmptyVec::try_new(patches)
                    .map_err(|error| io::Error::other(error.to_string()))?,
            },
        )
    }
}

fn settings_patches(
    config: &RunConfig,
    snapshot: &crate::surface::SurfaceSnapshot,
) -> io::Result<Vec<RuntimeSettingsPatch>> {
    let mut patches = Vec::new();
    let approval_mode = match config.approval_mode {
        orca_core::approval_types::ApprovalMode::Suggest => {
            crate::surface::SurfaceApprovalMode::Suggest
        }
        orca_core::approval_types::ApprovalMode::AutoEdit => {
            crate::surface::SurfaceApprovalMode::AutoEdit
        }
        orca_core::approval_types::ApprovalMode::FullAuto => {
            crate::surface::SurfaceApprovalMode::FullAuto
        }
        orca_core::approval_types::ApprovalMode::Plan => crate::surface::SurfaceApprovalMode::Plan,
    };
    if snapshot.settings.effective.approval_mode != approval_mode {
        patches.push(RuntimeSettingsPatch::SetApprovalMode {
            mode: approval_mode,
        });
    }
    if let Some(roots) = config.runtime_workspace_roots.as_ref() {
        let roots = roots
            .iter()
            .cloned()
            .map(crate::surface::CanonicalPath::try_new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io::Error::other(error.to_string()))?;
        if snapshot.settings.effective.workspace_roots != roots {
            patches.push(RuntimeSettingsPatch::SetWorkspaceRoots { roots });
        }
    }
    let profile = config
        .active_permission_profile
        .as_ref()
        .map(|profile| {
            Ok(crate::surface::SurfaceActivePermissionProfile {
                id: crate::surface::NonEmptyText::try_new(profile.id.clone())?,
                extends: profile
                    .extends
                    .as_ref()
                    .map(|value| crate::surface::NonEmptyText::try_new(value.clone()))
                    .transpose()?,
            })
        })
        .transpose()
        .map_err(|error: crate::surface::SurfaceValueError| io::Error::other(error.to_string()))?;
    if snapshot.settings.effective.active_permission_profile != profile {
        patches.push(RuntimeSettingsPatch::SetActivePermissionProfile { profile });
    }
    let rules = config
        .permission_rules
        .rules
        .iter()
        .map(|rule| {
            Ok(crate::surface::SurfacePermissionRule {
                tool: crate::surface::NonEmptyText::try_new(rule.tool.clone())?,
                pattern: crate::surface::NonEmptyText::try_new(rule.pattern.clone())?,
                decision: match rule.decision {
                    orca_core::approval_types::Decision::Allow => {
                        crate::surface::SurfacePermissionDecision::Allow
                    }
                    orca_core::approval_types::Decision::Prompt => {
                        crate::surface::SurfacePermissionDecision::Prompt
                    }
                    orca_core::approval_types::Decision::Deny => {
                        crate::surface::SurfacePermissionDecision::Deny
                    }
                },
            })
        })
        .collect::<Result<Vec<_>, crate::surface::SurfaceValueError>>()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if snapshot.settings.effective.permission_rules.ordered_rules != rules {
        patches.push(RuntimeSettingsPatch::ReplacePermissionRules { rules });
    }
    let directories = config
        .additional_working_directories
        .iter()
        .map(|directory| {
            Ok(crate::surface::SurfaceAdditionalWorkingDirectory {
                path: crate::surface::CanonicalPath::try_new(directory.path.clone())?,
                source: crate::surface::NonEmptyText::try_new(directory.source.clone())?,
            })
        })
        .collect::<Result<Vec<_>, crate::surface::SurfaceValueError>>()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if snapshot.settings.effective.additional_working_directories != directories {
        patches.push(RuntimeSettingsPatch::ReplaceAdditionalWorkingDirectories { directories });
    }
    Ok(patches)
}

fn committed<T>(
    result: Result<MutationReply<T>, crate::surface::SurfaceClientCommandError>,
    action: &str,
) -> io::Result<T> {
    match result.map_err(|error| io::Error::other(format!("{action} failed: {error:?}")))? {
        MutationReply::Committed { value, .. } => Ok(value),
        MutationReply::Deferred { mutation, .. } => Err(io::Error::other(format!(
            "{action} deferred: request={:?} commit={:?}",
            mutation.request_id, mutation.commit_id
        ))),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "{action} did not commit: {}",
            uncommitted_message(&mutation)
        ))),
    }
}

fn uncommitted_message(mutation: &UncommittedMutation) -> &str {
    match mutation {
        UncommittedMutation::Invalid { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Stale { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Unavailable { error, .. } => error.error().message.as_str(),
        UncommittedMutation::CommitFailed { error, .. } => error.error().message.as_str(),
    }
}

fn jsonl_thread_config(config: &RunConfig) -> RunConfig {
    let mut config = config.clone();
    if let Some(roots) = config.runtime_workspace_roots.as_mut() {
        for root in roots {
            if let Ok(canonical) = std::fs::canonicalize(&*root) {
                *root = canonical;
            }
        }
    }
    config.output_format = OutputFormat::Jsonl;
    config.history_mode = HistoryMode::Record;
    config.show_session_picker = false;
    config.desktop_notifications = false;
    config
}

fn runtime_host_error(error: RuntimeHostError) -> io::Error {
    io::Error::other(error.to_string())
}

fn jsonl_turn_interaction_capabilities() -> BTreeSet<SurfaceInteractionKind> {
    BTreeSet::from([
        SurfaceInteractionKind::ToolApproval,
        SurfaceInteractionKind::PermissionRequest,
        SurfaceInteractionKind::UserInput,
        SurfaceInteractionKind::McpElicitation,
    ])
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::time::Duration;

    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig,
        WorkflowConfig,
    };
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use tempfile::tempdir;

    use super::*;
    use crate::runtime_host::{
        GenerationContext, HostedTurnRequest, ThreadOperationExecutor, ThreadOperationOutcome,
    };
    use crate::server::opaque_permission_router::JsonlConnectionAdmission;
    use crate::thread::RuntimeThread;

    const PROJECTION_FAILURE: &str = "injected JSONL projection disconnect";

    #[test]
    fn server_turn_attachments_declare_four_interaction_capabilities() {
        assert_eq!(
            jsonl_turn_interaction_capabilities(),
            BTreeSet::from([
                SurfaceInteractionKind::ToolApproval,
                SurfaceInteractionKind::PermissionRequest,
                SurfaceInteractionKind::UserInput,
                SurfaceInteractionKind::McpElicitation,
            ])
        );
    }

    struct CancelAwareExecutor {
        entered: SyncSender<()>,
        cancel_observed: SyncSender<()>,
        completed: SyncSender<()>,
    }

    impl ThreadOperationExecutor for CancelAwareExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.entered
                .send(())
                .map_err(|_| io::Error::other("projection test entry receiver closed"))?;
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.cancel_observed
                .send(())
                .map_err(|_| io::Error::other("projection test cancel receiver closed"))?;
            thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
            self.completed
                .send(())
                .map_err(|_| io::Error::other("projection test completion receiver closed"))?;
            Ok(RunStatus::Cancelled.into())
        }
    }

    struct BrokenProjectionWriter;

    impl Write for BrokenProjectionWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                PROJECTION_FAILURE,
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl HostedOperationWriter for BrokenProjectionWriter {
        fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
            Ok(())
        }
    }

    impl JsonlSurfaceOutput for BrokenProjectionWriter {}

    #[test]
    fn projection_write_failure_is_returned_after_worker_and_ephemeral_actor_cleanup() {
        let (entered_tx, entered_rx) = sync_channel(1);
        let (cancel_tx, cancel_rx) = sync_channel(1);
        let (completed_tx, completed_rx) = sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(CancelAwareExecutor {
            entered: entered_tx,
            cancel_observed: cancel_tx,
            completed: completed_tx,
        }))
        .expect("start projection failure runtime host");
        let surface_host = host.surface_handle().bind_new_connection();
        let mut adapter = JsonlSurfaceAdapter {
            host: Some(host),
            surface_host,
            threads: HashMap::new(),
            ephemeral_threads: Arc::new(Mutex::new(HashMap::new())),
            transport_turns: Vec::new(),
        };
        let cwd = tempdir().expect("projection test cwd");
        let config = test_run_config(cwd.path().to_path_buf());
        let prepared = adapter
            .prepare_stateless_turn_with_interactions(
                &config,
                "wait for projection disconnect",
                PermissionProfileOverride::default(),
                &serde_json::json!("projection-failure"),
                test_interactions(),
            )
            .expect("prepare stateless projection failure turn");
        let thread_id = prepared.thread_id().to_string();
        let ephemeral_handle = adapter
            .ephemeral_thread(&thread_id)
            .expect("ephemeral runtime is registered while its projection is active");
        let mut turn = prepared
            .start_with_output(BrokenProjectionWriter)
            .expect("admit projection failure turn");

        expect_signal(entered_rx, "generation did not start");
        let error = turn
            .wait_terminal()
            .expect_err("projection disconnect must be returned to the transport owner");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), PROJECTION_FAILURE);
        assert!(turn.worker.is_none(), "projection worker was not joined");
        expect_signal(
            cancel_rx,
            "generation did not observe projection cancellation",
        );
        expect_signal(completed_rx, "generation was not joined after cancellation");
        assert!(
            adapter
                .ephemeral_threads
                .lock()
                .expect("registry")
                .is_empty(),
            "projection failure retained an ephemeral registry entry"
        );
        assert!(
            ephemeral_handle
                .jsonl_read_live_projection(false, false)
                .is_err(),
            "projection failure left its ephemeral actor available"
        );

        let (shutdown_tx, shutdown_rx) = sync_channel(1);
        let shutdown_worker = std::thread::spawn(move || {
            let _ = shutdown_tx.send(adapter.shutdown());
        });
        let shutdown = shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("runtime host shutdown hung after projection failure");
        assert!(
            shutdown.is_ok(),
            "runtime host shutdown failed: {shutdown:?}"
        );
        shutdown_worker.join().expect("join runtime host shutdown");
    }

    fn expect_signal(receiver: Receiver<()>, failure: &str) {
        receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("{failure}"));
    }

    fn test_interactions() -> JsonlInteractionTransport {
        let admission = JsonlConnectionAdmission::new_ephemeral();
        JsonlInteractionTransport::new(
            JsonlOpaquePermissionRouter::new(admission.clone()),
            JsonlDirectInteractionAdapter::new(admission),
        )
    }

    fn test_run_config(cwd: std::path::PathBuf) -> RunConfig {
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
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("test model"),
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
}
