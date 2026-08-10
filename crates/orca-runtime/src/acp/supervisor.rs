use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use agent_client_protocol::{
    Agent, AuthenticateRequest, CancelNotification, CreateTerminalRequest, CreateTerminalResponse,
    EnvVariable, InitializeRequest, KillTerminalRequest, KillTerminalResponse, LoadSessionRequest,
    NewSessionRequest, PromptRequest, ReadTextFileRequest, ReadTextFileResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, RequestPermissionResponse, SessionId,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use orca_core::config::RunConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, oneshot};

use super::agent::{
    ACP_NOTIFICATION_CAPACITY, AcpClientBridge, AcpNotificationDelivery, AcpPermissionWaitError,
    OrcaAcpAgent,
};
use super::rpc_facade::{
    FrameDirection, InboundFrame, LocalHandlerCompletion, LocalHandlerFuture,
    ResponseSessionResolver, RpcFacadeConfig, RpcFacadeError, RpcFacadeHandle, TransportFrame,
    spawn_local_rpc_facade_with_response_session_resolver,
};
use crate::runtime_surface::{
    AcpReadTextFileSettlement, AcpTerminalCleanupSettlement, AcpTerminalCreateSettlement,
    AcpTerminalObservationSettlement, AcpWriteTextFileSettlement, CapabilityRevision, NonEmptyText,
    SurfaceCapabilityCallId, SurfaceCapabilityCallKind, SurfaceTerminalExitStatus,
};
use crate::surface::{RuntimeSurfaceClientHandle, RuntimeSurfaceHostHandle};

const ACP_REVERSE_REQUEST_DEADLINE: Duration = Duration::from_secs(120);

struct PendingPermissionRoute {
    session_id: SessionId,
    key: String,
    completed: oneshot::Sender<()>,
}

#[derive(Default)]
struct PermissionRoutes {
    next_request_id: Cell<i64>,
    pending: Arc<Mutex<HashMap<i64, PendingPermissionRoute>>>,
}

struct PendingReadTextFileRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    client: RuntimeSurfaceClientHandle,
    physically_written: bool,
    completed: oneshot::Sender<AcpReadTextFileSettlement>,
}

#[derive(Default)]
struct ReadTextFileRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingReadTextFileRoute>>>,
}

struct WriteTextFileRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingWriteTextFileRoute>>>,
    response_observer: Option<Arc<Notify>>,
    written_observer: Option<Arc<Notify>>,
}

impl Default for WriteTextFileRoutes {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            response_observer: None,
            written_observer: None,
        }
    }
}

struct PendingWriteTextFileRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    client: RuntimeSurfaceClientHandle,
    delivery_possible: bool,
    completed: oneshot::Sender<AcpWriteTextFileSettlement>,
}

#[derive(Default)]
struct TerminalCreateRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingTerminalCreateRoute>>>,
}

struct PendingTerminalCreateRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    client: RuntimeSurfaceClientHandle,
    delivery_possible: bool,
    completed: oneshot::Sender<AcpTerminalCreateSettlement>,
}

#[derive(Default)]
struct TerminalObservationRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingTerminalObservationRoute>>>,
}

struct PendingTerminalObservationRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    client: RuntimeSurfaceClientHandle,
    kind: SurfaceCapabilityCallKind,
    physically_written: bool,
    completed: oneshot::Sender<AcpTerminalObservationSettlement>,
}

struct TerminalCleanupRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingTerminalCleanupRoute>>>,
    response_observer: Option<Arc<Notify>>,
}

impl Default for TerminalCleanupRoutes {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            response_observer: None,
        }
    }
}

struct PendingTerminalCleanupRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    kind: SurfaceCapabilityCallKind,
    client: RuntimeSurfaceClientHandle,
    completed: oneshot::Sender<AcpTerminalCleanupSettlement>,
}

struct CapabilityRequestIds {
    next_request_id: Cell<i64>,
}

impl Default for CapabilityRequestIds {
    fn default() -> Self {
        Self {
            next_request_id: Cell::new(-1),
        }
    }
}

impl CapabilityRequestIds {
    fn reserve(&self) -> Option<i64> {
        let request_id = self.next_request_id.get();
        self.next_request_id.set(request_id.checked_sub(1)?);
        Some(request_id)
    }
}

pub(crate) async fn run_connection<R, W>(
    surface_host: RuntimeSurfaceHostHandle,
    config: RunConfig,
    reader: R,
    writer: W,
) -> Result<(), RpcFacadeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run_connection_inner(surface_host, config, reader, writer, None, None).await
}

async fn run_connection_inner<R, W>(
    surface_host: RuntimeSurfaceHostHandle,
    config: RunConfig,
    reader: R,
    writer: W,
    write_response_observer: Option<Arc<Notify>>,
    write_written_observer: Option<Arc<Notify>>,
) -> Result<(), RpcFacadeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (notification_tx, notification_rx) = tokio::sync::mpsc::channel(ACP_NOTIFICATION_CAPACITY);
    let (
        client_bridge,
        permission_rx,
        read_text_file_rx,
        write_text_file_rx,
        terminal_create_rx,
        terminal_observation_rx,
        terminal_cleanup_rx,
    ) = AcpClientBridge::new_with_capability_lanes();
    let agent = Rc::new(
        OrcaAcpAgent::new_supervised(surface_host, config, notification_tx)
            .with_client_bridge(Arc::clone(&client_bridge)),
    );
    let facade_slot = Rc::new(RefCell::new(None::<RpcFacadeHandle>));
    let permission_routes = Rc::new(PermissionRoutes::default());
    let read_text_file_routes = Rc::new(ReadTextFileRoutes::default());
    let write_text_file_routes = Rc::new(WriteTextFileRoutes {
        response_observer: write_response_observer.clone(),
        written_observer: write_written_observer,
        ..WriteTextFileRoutes::default()
    });
    let terminal_create_routes = Rc::new(TerminalCreateRoutes::default());
    let terminal_observation_routes = Rc::new(TerminalObservationRoutes::default());
    let terminal_cleanup_routes = Rc::new(TerminalCleanupRoutes {
        response_observer: write_response_observer,
        ..TerminalCleanupRoutes::default()
    });
    let capability_request_ids = Rc::new(CapabilityRequestIds::default());
    let handler = {
        let agent = Rc::clone(&agent);
        let facade_slot = Rc::clone(&facade_slot);
        let client_bridge = Arc::clone(&client_bridge);
        let permission_routes = Rc::clone(&permission_routes);
        let read_text_file_routes = Rc::clone(&read_text_file_routes);
        let write_text_file_routes = Rc::clone(&write_text_file_routes);
        let terminal_create_routes = Rc::clone(&terminal_create_routes);
        let terminal_observation_routes = Rc::clone(&terminal_observation_routes);
        let terminal_cleanup_routes = Rc::clone(&terminal_cleanup_routes);
        Rc::new(move |frame: InboundFrame| {
            handle_inbound(
                Rc::clone(&agent),
                Rc::clone(&facade_slot),
                Arc::clone(&client_bridge),
                Rc::clone(&permission_routes),
                Rc::clone(&read_text_file_routes),
                Rc::clone(&write_text_file_routes),
                Rc::clone(&terminal_create_routes),
                Rc::clone(&terminal_observation_routes),
                Rc::clone(&terminal_cleanup_routes),
                frame,
            )
        })
    };
    let response_routes = Arc::clone(&permission_routes.pending);
    let read_response_routes = Arc::clone(&read_text_file_routes.pending);
    let write_response_routes = Arc::clone(&write_text_file_routes.pending);
    let terminal_create_response_routes = Arc::clone(&terminal_create_routes.pending);
    let terminal_observation_response_routes = Arc::clone(&terminal_observation_routes.pending);
    let terminal_cleanup_response_routes = Arc::clone(&terminal_cleanup_routes.pending);
    let response_session_resolver: ResponseSessionResolver = Arc::new(move |request_id| {
        response_routes
            .lock()
            .expect("ACP permission route mutex is not poisoned")
            .get(&request_id)
            .map(|route| route.session_id.to_string())
            .or_else(|| {
                read_response_routes
                    .lock()
                    .expect("ACP read route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
            .or_else(|| {
                write_response_routes
                    .lock()
                    .expect("ACP write route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
            .or_else(|| {
                terminal_create_response_routes
                    .lock()
                    .expect("ACP terminal create route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
            .or_else(|| {
                terminal_observation_response_routes
                    .lock()
                    .expect("ACP terminal observation route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
            .or_else(|| {
                terminal_cleanup_response_routes
                    .lock()
                    .expect("ACP terminal cleanup route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
    });
    let (facade, supervisor) = spawn_local_rpc_facade_with_response_session_resolver(
        reader,
        writer,
        handler,
        response_session_resolver,
        RpcFacadeConfig::default(),
    );
    *facade_slot.borrow_mut() = Some(facade.clone());

    let notification_task =
        tokio::task::spawn_local(dispatch_notifications(facade.clone(), notification_rx));
    let permission_task = tokio::task::spawn_local(dispatch_permissions(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&permission_routes),
        permission_rx,
    ));
    let mut read_text_file_task = tokio::task::spawn_local(dispatch_read_text_files(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&read_text_file_routes),
        Rc::clone(&capability_request_ids),
        read_text_file_rx,
    ));
    let mut write_text_file_task = tokio::task::spawn_local(dispatch_write_text_files(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&write_text_file_routes),
        Rc::clone(&capability_request_ids),
        write_text_file_rx,
    ));
    let mut terminal_create_task = tokio::task::spawn_local(dispatch_terminal_creates(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&terminal_create_routes),
        Rc::clone(&capability_request_ids),
        terminal_create_rx,
    ));
    let mut terminal_observation_task = tokio::task::spawn_local(dispatch_terminal_observations(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&terminal_observation_routes),
        Rc::clone(&capability_request_ids),
        terminal_observation_rx,
    ));
    let mut terminal_cleanup_task = tokio::task::spawn_local(dispatch_terminal_cleanups(
        facade,
        Arc::clone(&client_bridge),
        Rc::clone(&terminal_cleanup_routes),
        capability_request_ids,
        terminal_cleanup_rx,
    ));

    let result = supervisor.wait().await.map(|_| ());
    client_bridge.cancel_all();
    retire_all_permission_routes(&permission_routes);
    retire_all_read_text_file_routes(&read_text_file_routes);
    retire_all_write_text_file_routes(&write_text_file_routes);
    retire_all_terminal_create_routes(&terminal_create_routes);
    retire_all_terminal_observation_routes(&terminal_observation_routes);
    retire_all_terminal_cleanup_routes(&terminal_cleanup_routes);
    notification_task.abort();
    permission_task.abort();
    let _ = notification_task.await;
    let _ = permission_task.await;
    if tokio::time::timeout(Duration::from_secs(5), &mut read_text_file_task)
        .await
        .is_err()
    {
        read_text_file_task.abort();
        let _ = read_text_file_task.await;
    }
    if tokio::time::timeout(Duration::from_secs(5), &mut write_text_file_task)
        .await
        .is_err()
    {
        write_text_file_task.abort();
        let _ = write_text_file_task.await;
    }
    if tokio::time::timeout(Duration::from_secs(5), &mut terminal_create_task)
        .await
        .is_err()
    {
        terminal_create_task.abort();
        let _ = terminal_create_task.await;
    }
    if tokio::time::timeout(Duration::from_secs(5), &mut terminal_observation_task)
        .await
        .is_err()
    {
        terminal_observation_task.abort();
        let _ = terminal_observation_task.await;
    }
    if tokio::time::timeout(Duration::from_secs(5), &mut terminal_cleanup_task)
        .await
        .is_err()
    {
        terminal_cleanup_task.abort();
        let _ = terminal_cleanup_task.await;
    }
    result
}

fn handle_inbound(
    agent: Rc<OrcaAcpAgent>,
    facade_slot: Rc<RefCell<Option<RpcFacadeHandle>>>,
    client_bridge: Arc<AcpClientBridge>,
    permission_routes: Rc<PermissionRoutes>,
    read_text_file_routes: Rc<ReadTextFileRoutes>,
    write_text_file_routes: Rc<WriteTextFileRoutes>,
    terminal_create_routes: Rc<TerminalCreateRoutes>,
    terminal_observation_routes: Rc<TerminalObservationRoutes>,
    terminal_cleanup_routes: Rc<TerminalCleanupRoutes>,
    frame: InboundFrame,
) -> LocalHandlerFuture {
    Box::pin(async move {
        let value = frame.json_value()?;
        if frame.method().is_none() {
            if !handle_read_text_file_response(&read_text_file_routes, &value)
                && !handle_write_text_file_response(&write_text_file_routes, &value)
                && !handle_terminal_create_response(&terminal_create_routes, &value)
                && !handle_terminal_observation_response(&terminal_observation_routes, &value)
                && !handle_terminal_cleanup_response(&terminal_cleanup_routes, &value)
            {
                handle_permission_response(&client_bridge, &permission_routes, &value);
            }
            return Ok(empty_completion());
        }
        let method = frame.method().expect("checked method").to_string();
        let request_id = value.get("id").cloned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let facade = facade_slot
            .borrow()
            .as_ref()
            .cloned()
            .ok_or(RpcFacadeError::Sealed)?;

        match method.as_str() {
            "initialize" => {
                let result = decode::<InitializeRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::initialize(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "authenticate" => {
                let result = decode::<AuthenticateRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::authenticate(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "session/new" => {
                let result = decode::<NewSessionRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::new_session(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "session/load" => {
                let result = decode::<LoadSessionRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::load_session(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "session/prompt" => {
                let result = match decode::<PromptRequest>(params) {
                    Ok(args) => {
                        let inbound_sequence = frame.session_sequence().ok_or_else(|| {
                            agent_client_protocol::Error::invalid_params()
                                .data("ACP prompt is missing a session sequence")
                        });
                        match inbound_sequence {
                            Ok(inbound_sequence) => {
                                agent.admit_prompt(args, Some(inbound_sequence)).await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(agent_client_protocol::Error::invalid_params()
                        .data(format!("invalid ACP prompt: {error}"))),
                };
                match result {
                    Ok(admitted) => Ok(Box::pin(async move {
                        let result = agent.complete_prompt(admitted).await;
                        let _ = send_response(&facade, request_id, result).await;
                    }) as LocalHandlerCompletion),
                    Err(error) => Ok(response_completion::<Value>(facade, request_id, Err(error))),
                }
            }
            "session/cancel" => {
                let args: CancelNotification =
                    decode(params).map_err(|error| RpcFacadeError::Protocol {
                        message: format!("invalid ACP cancel: {error}"),
                    })?;
                let session_id = args.session_id.clone();
                Agent::cancel(agent.as_ref(), args).await.map_err(|error| {
                    RpcFacadeError::Protocol {
                        message: format!("ACP cancel failed: {error:?}"),
                    }
                })?;
                retire_session_permission_routes(&client_bridge, &permission_routes, &session_id);
                retire_session_read_text_file_routes(&read_text_file_routes, &session_id);
                retire_session_write_text_file_routes(&write_text_file_routes, &session_id);
                retire_session_terminal_create_routes(&terminal_create_routes, &session_id);
                retire_session_terminal_observation_routes(
                    &terminal_observation_routes,
                    &session_id,
                );
                retire_session_terminal_cleanup_routes(&terminal_cleanup_routes, &session_id);
                Ok(empty_completion())
            }
            _ => {
                let error = agent_client_protocol::Error::method_not_found()
                    .data(format!("unsupported ACP method '{method}'"));
                Ok(response_completion::<Value>(facade, request_id, Err(error)))
            }
        }
    })
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value)
}

fn empty_completion() -> LocalHandlerCompletion {
    Box::pin(async {})
}

fn response_completion<T>(
    facade: RpcFacadeHandle,
    request_id: Option<Value>,
    result: Result<T, agent_client_protocol::Error>,
) -> LocalHandlerCompletion
where
    T: Serialize + 'static,
{
    Box::pin(async move {
        let _ = send_response(&facade, request_id, result).await;
    })
}

async fn send_response<T>(
    facade: &RpcFacadeHandle,
    request_id: Option<Value>,
    result: Result<T, agent_client_protocol::Error>,
) -> Result<(), RpcFacadeError>
where
    T: Serialize,
{
    let Some(request_id) = request_id else {
        return Ok(());
    };
    let value = match result {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result,
        }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": error,
        }),
    };
    enqueue_json(facade, value).await
}

async fn dispatch_notifications(
    facade: RpcFacadeHandle,
    mut notifications: tokio::sync::mpsc::Receiver<AcpNotificationDelivery>,
) {
    while let Some(delivery) = notifications.recv().await {
        let value = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": delivery.notification,
        });
        let result = enqueue_json(&facade, value)
            .await
            .map_err(|error| error.to_string());
        let failed = result.is_err();
        let _ = delivery.acknowledgement.send(result);
        if failed {
            break;
        }
    }
}

async fn dispatch_permissions(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<PermissionRoutes>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpPermissionRequest>,
) {
    while let Some(request) = requests.recv().await {
        if !client_bridge.is_pending(&request.key) {
            continue;
        }
        let request_id = routes.next_request_id.get();
        let Some(next_request_id) = request_id.checked_add(1) else {
            client_bridge.complete_permission(
                &request.key,
                Err(AcpPermissionWaitError::Client(
                    "ACP reverse request id exhausted".to_string(),
                )),
            );
            break;
        };
        routes.next_request_id.set(next_request_id);
        let (completed, completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP permission route mutex is not poisoned")
            .insert(
                request_id,
                PendingPermissionRoute {
                    session_id: request.request.session_id.clone(),
                    key: request.key.clone(),
                    completed,
                },
            );
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/request_permission",
            "params": request.request,
        });
        if let Err(error) = enqueue_json(&facade, value).await {
            routes
                .pending
                .lock()
                .expect("ACP permission route mutex is not poisoned")
                .remove(&request_id);
            client_bridge.complete_permission(
                &request.key,
                Err(AcpPermissionWaitError::Client(error.to_string())),
            );
            break;
        }
        if tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion)
            .await
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP permission route mutex is not poisoned")
                .remove(&request_id);
            client_bridge.complete_permission(
                &request.key,
                Err(AcpPermissionWaitError::Client(
                    "ACP permission response timed out".to_string(),
                )),
            );
        }
    }
}

async fn dispatch_read_text_files(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<ReadTextFileRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpReadTextFileRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::FailedBeforeWrite {
                    message: "ACP read reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let params = ReadTextFileRequest::new(
            session_id.clone(),
            request.dispatch.path.as_path().to_path_buf(),
        )
        .line(request.dispatch.line)
        .limit(request.dispatch.limit);
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP read route mutex is not poisoned")
            .insert(
                request_id,
                PendingReadTextFileRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    client: request.client.clone(),
                    physically_written: false,
                    completed,
                },
            );
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "fs/read_text_file",
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP read route mutex is not poisoned")
                    .remove(&request_id);
                let _ = request.client.settle_acp_read_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpReadTextFileSettlement::FailedBeforeWrite {
                        message: format!("ACP read request could not be encoded: {error}"),
                    },
                );
                break;
            }
        };
        encoded.push(b'\n');
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::FailedBeforeWrite {
                    message: "ACP read request was cancelled before write".to_string(),
                },
            );
            continue;
        }
        if request
            .client
            .claim_acp_read_text_file_write(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            client_bridge.finish_capability_write(&session_id);
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message: "ACP read durable write claim could not be confirmed".to_string(),
                },
            );
            continue;
        }
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP read route mutex is not poisoned")
            .get_mut(&request_id)
        {
            // The durable claim is the conservative delivery-possible barrier.
            route.physically_written = true;
        }
        let write_receipt =
            match facade.enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded)) {
                Ok(receipt) => receipt,
                Err(error) => {
                    client_bridge.finish_capability_write(&session_id);
                    routes
                        .pending
                        .lock()
                        .expect("ACP read route mutex is not poisoned")
                        .remove(&request_id);
                    let _ = request.client.settle_acp_read_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpReadTextFileSettlement::ObservationUnavailable {
                        message: format!(
                            "ACP read request was rejected after delivery became possible: {error}"
                        ),
                    },
                );
                    break;
                }
            };
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let settlement = completion.try_recv().unwrap_or_else(|_| {
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message: format!(
                        "ACP read request may have been written but acknowledgement failed: {error}"
                    ),
                }
            });
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                settlement,
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_read_text_file_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message:
                        "ACP read request was written but its durable write acknowledgement failed"
                            .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpReadTextFileSettlement::ObservationUnavailable {
                message: "ACP read response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP read route mutex is not poisoned")
                    .remove(&request_id);
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message: "ACP read response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_read_text_file(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
}

async fn dispatch_write_text_files(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<WriteTextFileRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpWriteTextFileRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let params = WriteTextFileRequest::new(
            session_id.clone(),
            request.dispatch.path.as_path().to_path_buf(),
            request.dispatch.content,
        );
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "fs/write_text_file",
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpWriteTextFileSettlement::FailedBeforeWrite {
                        message: format!("ACP write request could not be encoded: {error}"),
                    },
                );
                continue;
            }
        };
        encoded.push(b'\n');
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP write route mutex is not poisoned")
            .insert(
                request_id,
                PendingWriteTextFileRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    client: request.client.clone(),
                    delivery_possible: false,
                    completed,
                },
            );
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write request was cancelled before delivery".to_string(),
                },
            );
            continue;
        }
        if request
            .client
            .permit_acp_write_text_file_delivery(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            client_bridge.finish_capability_write(&session_id);
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP write delivery barrier could not be observed after runtime admission"
                            .to_string(),
                },
            );
            continue;
        }
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP write route mutex is not poisoned")
            .get_mut(&request_id)
        {
            route.delivery_possible = true;
        }
        let write_receipt =
            match facade.enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded)) {
                Ok(receipt) => receipt,
                Err(error) => {
                    client_bridge.finish_capability_write(&session_id);
                    routes
                        .pending
                        .lock()
                        .expect("ACP write route mutex is not poisoned")
                        .remove(&request_id);
                    let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP write request was rejected after delivery became possible: {error}"
                        ),
                    },
                );
                    break;
                }
            };
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            if let Ok(settlement) = completion.try_recv() {
                // A decoded response proves that the request reached the peer
                // even when the local write/flush acknowledgement failed.
                // Establish (or retain) Written before forwarding that exact
                // response so the runtime owner cannot reject and lose it.
                let _ = request.client.mark_acp_write_text_file_written(
                    request.dispatch.call_id.clone(),
                    request.dispatch.capability_revision,
                );
                let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    settlement,
                );
            } else {
                let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP file write may have occurred but acknowledgement failed: {error}"
                        ),
                    },
                );
            }
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_write_text_file_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP file write completed but its durable acknowledgement was unavailable"
                            .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if let Some(observer) = &routes.written_observer {
            observer.notify_one();
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: "ACP write response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP write route mutex is not poisoned")
                    .remove(&request_id);
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message: "ACP write response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_write_text_file(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
}

async fn dispatch_terminal_creates(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<TerminalCreateRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpTerminalCreateRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_terminal_create(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCreateSettlement::FailedBeforeWrite {
                    message: "ACP terminal create reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let mut params =
            CreateTerminalRequest::new(session_id.clone(), request.dispatch.command.clone())
                .args(request.dispatch.args.clone())
                .env(
                    request
                        .dispatch
                        .env
                        .iter()
                        .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
                        .collect(),
                )
                .output_byte_limit(request.dispatch.output_byte_limit);
        if let Some(cwd) = request.dispatch.cwd.as_ref() {
            params = params.cwd(cwd.as_path().to_path_buf());
        }
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "terminal/create",
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = request.client.settle_acp_terminal_create(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCreateSettlement::FailedBeforeWrite {
                        message: format!(
                            "ACP terminal create request could not be encoded: {error}"
                        ),
                    },
                );
                continue;
            }
        };
        encoded.push(b'\n');
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP terminal create route mutex is not poisoned")
            .insert(
                request_id,
                PendingTerminalCreateRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    client: request.client.clone(),
                    delivery_possible: false,
                    completed,
                },
            );
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP terminal create route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_create(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCreateSettlement::FailedBeforeWrite {
                    message: "ACP terminal create was cancelled before delivery".to_string(),
                },
            );
            continue;
        }
        if request
            .client
            .permit_acp_terminal_create_delivery(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            client_bridge.finish_capability_write(&session_id);
            routes
                .pending
                .lock()
                .expect("ACP terminal create route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_create(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal create delivery barrier was unavailable after admission"
                        .to_string(),
                },
            );
            continue;
        }
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP terminal create route mutex is not poisoned")
            .get_mut(&request_id)
        {
            route.delivery_possible = true;
        }
        let write_receipt =
            match facade.enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded)) {
                Ok(receipt) => receipt,
                Err(error) => {
                    client_bridge.finish_capability_write(&session_id);
                    routes
                        .pending
                        .lock()
                        .expect("ACP terminal create route mutex is not poisoned")
                        .remove(&request_id);
                    let _ = request.client.settle_acp_terminal_create(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP terminal create may have occurred after enqueue failed: {error}"
                        ),
                    },
                );
                    break;
                }
            };
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP terminal create route mutex is not poisoned")
                .remove(&request_id);
            if let Ok(settlement) = completion.try_recv() {
                let _ = request.client.mark_acp_terminal_create_written(
                    request.dispatch.call_id.clone(),
                    request.dispatch.capability_revision,
                );
                let _ = request.client.settle_acp_terminal_create(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    settlement,
                );
            } else {
                let _ = request.client.settle_acp_terminal_create(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP terminal may exist but create acknowledgement failed: {error}"
                        ),
                    },
                );
            }
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_terminal_create_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP terminal create route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_create(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP terminal create completed but durable write acknowledgement was unavailable"
                            .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                message: "ACP terminal create response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP terminal create route mutex is not poisoned")
                    .remove(&request_id);
                AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal create response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_terminal_create(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
}

async fn dispatch_terminal_observations(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<TerminalObservationRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpTerminalObservationRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_terminal_observation(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalObservationSettlement::FailedBeforeWrite {
                    message: "ACP terminal observation reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let terminal_id = request.dispatch.terminal_id.as_str().to_string();
        let (method, params) = match request.dispatch.kind {
            SurfaceCapabilityCallKind::TerminalOutput => (
                "terminal/output",
                serde_json::to_value(TerminalOutputRequest::new(session_id.clone(), terminal_id)),
            ),
            SurfaceCapabilityCallKind::TerminalWaitForExit => (
                "terminal/wait_for_exit",
                serde_json::to_value(WaitForTerminalExitRequest::new(
                    session_id.clone(),
                    terminal_id,
                )),
            ),
            _ => {
                let _ = request.client.settle_acp_terminal_observation(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalObservationSettlement::FailedBeforeWrite {
                        message: "runtime dispatched an invalid terminal observation method"
                            .to_string(),
                    },
                );
                continue;
            }
        };
        let params = match params {
            Ok(params) => params,
            Err(error) => {
                let _ = request.client.settle_acp_terminal_observation(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalObservationSettlement::FailedBeforeWrite {
                        message: format!(
                            "ACP terminal observation request could not be encoded: {error}"
                        ),
                    },
                );
                continue;
            }
        };
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = request.client.settle_acp_terminal_observation(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalObservationSettlement::FailedBeforeWrite {
                        message: format!(
                            "ACP terminal observation request could not be encoded: {error}"
                        ),
                    },
                );
                continue;
            }
        };
        encoded.push(b'\n');
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP terminal observation route mutex is not poisoned")
            .insert(
                request_id,
                PendingTerminalObservationRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    client: request.client.clone(),
                    kind: request.dispatch.kind,
                    physically_written: false,
                    completed,
                },
            );
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP terminal observation route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_observation(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalObservationSettlement::FailedBeforeWrite {
                    message: "ACP terminal observation was cancelled before write".to_string(),
                },
            );
            continue;
        }
        if request
            .client
            .claim_acp_terminal_observation_write(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            client_bridge.finish_capability_write(&session_id);
            routes
                .pending
                .lock()
                .expect("ACP terminal observation route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_observation(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalObservationSettlement::ObservationUnavailable {
                    message: "ACP terminal observation durable write claim could not be confirmed"
                        .to_string(),
                },
            );
            continue;
        }
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP terminal observation route mutex is not poisoned")
            .get_mut(&request_id)
        {
            // The durable claim is the conservative delivery-possible barrier.
            route.physically_written = true;
        }
        let write_receipt = match facade
            .enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded))
        {
            Ok(receipt) => receipt,
            Err(error) => {
                client_bridge.finish_capability_write(&session_id);
                routes
                    .pending
                    .lock()
                    .expect("ACP terminal observation route mutex is not poisoned")
                    .remove(&request_id);
                let _ = request.client.settle_acp_terminal_observation(
                        request.dispatch.call_id,
                        request.dispatch.capability_revision,
                        AcpTerminalObservationSettlement::ObservationUnavailable {
                            message: format!(
                                "ACP terminal observation was rejected after delivery became possible: {error}"
                            ),
                        },
                    );
                break;
            }
        };
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP terminal observation route mutex is not poisoned")
                .remove(&request_id);
            let settlement = completion.try_recv().unwrap_or_else(|_| {
                AcpTerminalObservationSettlement::ObservationUnavailable {
                    message: format!(
                        "ACP terminal observation may have been written but acknowledgement failed: {error}"
                    ),
                }
            });
            let _ = request.client.settle_acp_terminal_observation(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                settlement,
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_terminal_observation_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP terminal observation route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_observation(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalObservationSettlement::ObservationUnavailable {
                    message: "ACP terminal observation was written but its durable acknowledgement failed"
                        .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpTerminalObservationSettlement::ObservationUnavailable {
                message: "ACP terminal observation response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP terminal observation route mutex is not poisoned")
                    .remove(&request_id);
                AcpTerminalObservationSettlement::ObservationUnavailable {
                    message: "ACP terminal observation response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_terminal_observation(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
}

async fn dispatch_terminal_cleanups(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<TerminalCleanupRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpTerminalCleanupRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_terminal_cleanup(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal cleanup reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let terminal_id = request.dispatch.terminal_id.as_str().to_string();
        let (method, params) = match request.dispatch.kind {
            SurfaceCapabilityCallKind::TerminalKill => (
                "terminal/kill",
                serde_json::to_value(KillTerminalRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                )),
            ),
            SurfaceCapabilityCallKind::TerminalRelease => (
                "terminal/release",
                serde_json::to_value(ReleaseTerminalRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                )),
            ),
            _ => {
                let _ = request.client.settle_acp_terminal_cleanup(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                        message: "runtime dispatched an invalid terminal cleanup method"
                            .to_string(),
                    },
                );
                continue;
            }
        };
        let params = match params {
            Ok(params) => params,
            Err(error) => {
                let _ = request.client.settle_acp_terminal_cleanup(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP terminal cleanup request could not be encoded: {error}"
                        ),
                    },
                );
                continue;
            }
        };
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = request.client.settle_acp_terminal_cleanup(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP terminal cleanup frame could not be encoded: {error}"
                        ),
                    },
                );
                continue;
            }
        };
        encoded.push(b'\n');
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP terminal cleanup route mutex is not poisoned")
            .insert(
                request_id,
                PendingTerminalCleanupRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    kind: request.dispatch.kind,
                    client: request.client.clone(),
                    completed,
                },
            );
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP terminal cleanup route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_cleanup(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal cleanup transport closed after durable admission"
                        .to_string(),
                },
            );
            continue;
        }
        let write_receipt =
            match facade.enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded)) {
                Ok(receipt) => receipt,
                Err(error) => {
                    client_bridge.finish_capability_write(&session_id);
                    routes
                        .pending
                        .lock()
                        .expect("ACP terminal cleanup route mutex is not poisoned")
                        .remove(&request_id);
                    let _ = request.client.settle_acp_terminal_cleanup(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP terminal cleanup may have occurred after enqueue failed: {error}"
                        ),
                    },
                );
                    break;
                }
            };
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP terminal cleanup route mutex is not poisoned")
                .remove(&request_id);
            if let Ok(settlement) = completion.try_recv() {
                let _ = request.client.mark_acp_terminal_cleanup_written(
                    request.dispatch.call_id.clone(),
                    request.dispatch.capability_revision,
                );
                let _ = request.client.settle_acp_terminal_cleanup(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    settlement,
                );
            } else {
                let _ = request.client.settle_acp_terminal_cleanup(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP terminal cleanup may have occurred but write acknowledgement failed: {error}"
                        ),
                    },
                );
            }
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_terminal_cleanup_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP terminal cleanup route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_terminal_cleanup(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal cleanup write acknowledgement could not be persisted"
                        .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                message: "ACP terminal cleanup response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP terminal cleanup route mutex is not poisoned")
                    .remove(&request_id);
                AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                    message: "ACP terminal cleanup response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_terminal_cleanup(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
    requests.close();
    while let Some(request) = requests.recv().await {
        let _ = request.client.settle_acp_terminal_cleanup(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                message: "ACP terminal cleanup transport closed before queued cleanup completed"
                    .to_string(),
            },
        );
    }
}

fn handle_read_text_file_response(routes: &ReadTextFileRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP read route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        match serde_json::from_value::<ReadTextFileResponse>(result.clone()) {
            Ok(response) => AcpReadTextFileSettlement::Completed {
                content: response.content,
            },
            Err(error) => AcpReadTextFileSettlement::ObservationUnavailable {
                message: format!("invalid ACP read response: {error}"),
            },
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP read request failed")
            .to_string();
        AcpReadTextFileSettlement::RemoteError { code, message }
    };
    let _ = route.completed.send(settlement);
    true
}

fn handle_write_text_file_response(routes: &WriteTextFileRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP write route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        match serde_json::from_value::<WriteTextFileResponse>(result.clone()) {
            Ok(_) => AcpWriteTextFileSettlement::Completed,
            Err(error) => AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: format!("invalid ACP write response: {error}"),
            },
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP write request failed")
            .to_string();
        AcpWriteTextFileSettlement::RemoteError { code, message }
    };
    let _ = route.completed.send(settlement);
    if let Some(observer) = &routes.response_observer {
        observer.notify_one();
    }
    true
}

fn handle_terminal_create_response(routes: &TerminalCreateRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP terminal create route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        match serde_json::from_value::<CreateTerminalResponse>(result.clone()) {
            Ok(response) => AcpTerminalCreateSettlement::Completed {
                terminal_id: response.terminal_id.to_string(),
            },
            Err(error) => AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                message: format!("invalid ACP terminal create response: {error}"),
            },
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP terminal create request failed")
            .to_string();
        AcpTerminalCreateSettlement::RemoteError { code, message }
    };
    let _ = route.completed.send(settlement);
    true
}

fn handle_terminal_observation_response(routes: &TerminalObservationRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP terminal observation route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        match route.kind {
            SurfaceCapabilityCallKind::TerminalOutput => {
                match serde_json::from_value::<TerminalOutputResponse>(result.clone())
                    .map_err(|error| error.to_string())
                    .and_then(|response| {
                        let exit_status = response
                            .exit_status
                            .map(surface_terminal_exit_status)
                            .transpose()?;
                        Ok((response.output, response.truncated, exit_status))
                    }) {
                    Ok((output, truncated, exit_status)) => {
                        AcpTerminalObservationSettlement::Output {
                            output,
                            truncated,
                            exit_status,
                        }
                    }
                    Err(error) => AcpTerminalObservationSettlement::ObservationUnavailable {
                        message: format!("invalid ACP terminal output response: {error}"),
                    },
                }
            }
            SurfaceCapabilityCallKind::TerminalWaitForExit => {
                match serde_json::from_value::<WaitForTerminalExitResponse>(result.clone())
                    .map_err(|error| error.to_string())
                    .and_then(|response| surface_terminal_exit_status(response.exit_status))
                {
                    Ok(exit_status) => AcpTerminalObservationSettlement::Exit { exit_status },
                    Err(error) => AcpTerminalObservationSettlement::ObservationUnavailable {
                        message: format!("invalid ACP terminal wait response: {error}"),
                    },
                }
            }
            _ => AcpTerminalObservationSettlement::ObservationUnavailable {
                message: "runtime routed an invalid terminal observation response".to_string(),
            },
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP terminal observation request failed")
            .to_string();
        AcpTerminalObservationSettlement::RemoteError { code, message }
    };
    let _ = route.completed.send(settlement);
    true
}

fn surface_terminal_exit_status(
    status: agent_client_protocol::TerminalExitStatus,
) -> Result<SurfaceTerminalExitStatus, String> {
    let signal = status
        .signal
        .map(NonEmptyText::try_new)
        .transpose()
        .map_err(|error| format!("invalid terminal exit signal: {error}"))?;
    Ok(SurfaceTerminalExitStatus {
        exit_code: status.exit_code,
        signal,
    })
}

fn handle_terminal_cleanup_response(routes: &TerminalCleanupRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP terminal cleanup route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        let valid = match route.kind {
            SurfaceCapabilityCallKind::TerminalKill => {
                serde_json::from_value::<KillTerminalResponse>(result.clone()).is_ok()
            }
            SurfaceCapabilityCallKind::TerminalRelease => {
                serde_json::from_value::<ReleaseTerminalResponse>(result.clone()).is_ok()
            }
            _ => false,
        };
        if valid {
            AcpTerminalCleanupSettlement::Completed
        } else {
            AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                message: "invalid ACP terminal cleanup response".to_string(),
            }
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP terminal cleanup request failed")
            .to_string();
        AcpTerminalCleanupSettlement::RemoteError { code, message }
    };
    if let Some(observer) = &routes.response_observer {
        observer.notify_waiters();
    }
    let _ = route.completed.send(settlement);
    true
}

fn handle_permission_response(bridge: &AcpClientBridge, routes: &PermissionRoutes, value: &Value) {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP permission route mutex is not poisoned")
        .remove(&request_id)
    else {
        return;
    };
    let result = if let Some(result) = value.get("result") {
        serde_json::from_value::<RequestPermissionResponse>(result.clone()).map_err(|error| {
            AcpPermissionWaitError::Client(format!("invalid ACP permission response: {error}"))
        })
    } else {
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP permission request failed")
            .to_string();
        Err(AcpPermissionWaitError::Client(message))
    };
    bridge.complete_permission(&route.key, result);
    let _ = route.completed.send(());
}

fn retire_session_permission_routes(
    bridge: &AcpClientBridge,
    routes: &PermissionRoutes,
    session_id: &SessionId,
) {
    let request_ids = routes
        .pending
        .lock()
        .expect("ACP permission route mutex is not poisoned")
        .iter()
        .filter_map(|(request_id, route)| (route.session_id == *session_id).then_some(*request_id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP permission route mutex is not poisoned")
            .remove(&request_id)
        {
            bridge.complete_permission(&route.key, Err(AcpPermissionWaitError::Cancelled));
            let _ = route.completed.send(());
        }
    }
}

fn retire_session_read_text_file_routes(routes: &ReadTextFileRoutes, session_id: &SessionId) {
    let request_ids = routes
        .pending
        .lock()
        .expect("ACP read route mutex is not poisoned")
        .iter()
        .filter_map(|(request_id, route)| (route.session_id == *session_id).then_some(*request_id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP read route mutex is not poisoned")
            .remove(&request_id)
        {
            let _ = route
                .completed
                .send(AcpReadTextFileSettlement::ObservationUnavailable {
                    message: "ACP read response route was cancelled".to_string(),
                });
        }
    }
}

fn retire_session_write_text_file_routes(routes: &WriteTextFileRoutes, session_id: &SessionId) {
    let request_ids = routes
        .pending
        .lock()
        .expect("ACP write route mutex is not poisoned")
        .iter()
        .filter_map(|(request_id, route)| (route.session_id == *session_id).then_some(*request_id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP write route mutex is not poisoned")
            .remove(&request_id)
        {
            let settlement = if route.delivery_possible {
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP write response route was cancelled after delivery became possible"
                            .to_string(),
                }
            } else {
                AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write response route was cancelled before delivery".to_string(),
                }
            };
            let _ = route.completed.send(settlement);
        }
    }
}

fn retire_session_terminal_create_routes(routes: &TerminalCreateRoutes, session_id: &SessionId) {
    let retired = {
        let mut pending = routes
            .pending
            .lock()
            .expect("ACP terminal create route mutex is not poisoned");
        let ids = pending
            .iter()
            .filter(|(_, route)| &route.session_id == session_id)
            .map(|(request_id, _)| *request_id)
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|request_id| pending.remove(&request_id))
            .collect::<Vec<_>>()
    };
    for route in retired {
        let settlement = if route.delivery_possible {
            AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                message: "ACP terminal create was cancelled after delivery became possible"
                    .to_string(),
            }
        } else {
            AcpTerminalCreateSettlement::FailedBeforeWrite {
                message: "ACP terminal create was cancelled before delivery".to_string(),
            }
        };
        let _ = route.client.settle_acp_terminal_create(
            route.call_id,
            route.capability_revision,
            settlement,
        );
    }
}

fn retire_session_terminal_observation_routes(
    routes: &TerminalObservationRoutes,
    session_id: &SessionId,
) {
    let retired = {
        let mut pending = routes
            .pending
            .lock()
            .expect("ACP terminal observation route mutex is not poisoned");
        let ids = pending
            .iter()
            .filter(|(_, route)| &route.session_id == session_id)
            .map(|(request_id, _)| *request_id)
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|request_id| pending.remove(&request_id))
            .collect::<Vec<_>>()
    };
    for route in retired {
        let settlement = if route.physically_written {
            AcpTerminalObservationSettlement::ObservationUnavailable {
                message: "ACP terminal observation was cancelled after write".to_string(),
            }
        } else {
            AcpTerminalObservationSettlement::FailedBeforeWrite {
                message: "ACP terminal observation was cancelled before write".to_string(),
            }
        };
        let _ = route.client.settle_acp_terminal_observation(
            route.call_id,
            route.capability_revision,
            settlement,
        );
    }
}

fn retire_session_terminal_cleanup_routes(routes: &TerminalCleanupRoutes, session_id: &SessionId) {
    let retired = {
        let mut pending = routes
            .pending
            .lock()
            .expect("ACP terminal cleanup route mutex is not poisoned");
        let ids = pending
            .iter()
            .filter(|(_, route)| &route.session_id == session_id)
            .map(|(request_id, _)| *request_id)
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|request_id| pending.remove(&request_id))
            .collect::<Vec<_>>()
    };
    for route in retired {
        let settlement = AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
            message: "ACP terminal cleanup was cancelled before response".to_string(),
        };
        let _ = route.client.settle_acp_terminal_cleanup(
            route.call_id,
            route.capability_revision,
            settlement,
        );
    }
}

fn retire_all_permission_routes(routes: &PermissionRoutes) {
    let pending = routes
        .pending
        .lock()
        .expect("ACP permission route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in pending {
        let _ = route.completed.send(());
    }
}

fn retire_all_read_text_file_routes(routes: &ReadTextFileRoutes) {
    let pending = routes
        .pending
        .lock()
        .expect("ACP read route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in pending {
        let durable_settlement = if route.physically_written {
            AcpReadTextFileSettlement::ObservationUnavailable {
                message: "ACP read response route was retired after write".to_string(),
            }
        } else {
            AcpReadTextFileSettlement::FailedBeforeWrite {
                message: "ACP read response route was retired before write".to_string(),
            }
        };
        // The dispatcher task is aborted immediately after connection teardown.
        // Settle through the runtime owner first so the durable call cannot be
        // stranded merely because this adapter task loses its final timeslice.
        let _ = route.client.settle_acp_read_text_file(
            route.call_id,
            route.capability_revision,
            durable_settlement,
        );
        let _ = route.completed.send(if route.physically_written {
            AcpReadTextFileSettlement::ObservationUnavailable {
                message: "ACP read response route was retired after write".to_string(),
            }
        } else {
            AcpReadTextFileSettlement::FailedBeforeWrite {
                message: "ACP read response route was retired before write".to_string(),
            }
        });
    }
}

fn retire_all_write_text_file_routes(routes: &WriteTextFileRoutes) {
    let pending = routes
        .pending
        .lock()
        .expect("ACP write route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in pending {
        let durable_settlement = if route.delivery_possible {
            AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: "ACP write response route was retired after delivery became possible"
                    .to_string(),
            }
        } else {
            AcpWriteTextFileSettlement::FailedBeforeWrite {
                message: "ACP write response route was retired before delivery".to_string(),
            }
        };
        let _ = route.client.settle_acp_write_text_file(
            route.call_id,
            route.capability_revision,
            durable_settlement,
        );
        let _ = route.completed.send(if route.delivery_possible {
            AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: "ACP write response route was retired after delivery became possible"
                    .to_string(),
            }
        } else {
            AcpWriteTextFileSettlement::FailedBeforeWrite {
                message: "ACP write response route was retired before delivery".to_string(),
            }
        });
    }
}

fn retire_all_terminal_create_routes(routes: &TerminalCreateRoutes) {
    let retired = routes
        .pending
        .lock()
        .expect("ACP terminal create route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in retired {
        let settlement = if route.delivery_possible {
            AcpTerminalCreateSettlement::ExternalEffectAmbiguous {
                message: "ACP terminal create connection closed after delivery became possible"
                    .to_string(),
            }
        } else {
            AcpTerminalCreateSettlement::FailedBeforeWrite {
                message: "ACP terminal create connection closed before delivery".to_string(),
            }
        };
        let _ = route.client.settle_acp_terminal_create(
            route.call_id,
            route.capability_revision,
            settlement,
        );
    }
}

fn retire_all_terminal_observation_routes(routes: &TerminalObservationRoutes) {
    let retired = routes
        .pending
        .lock()
        .expect("ACP terminal observation route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in retired {
        let settlement = if route.physically_written {
            AcpTerminalObservationSettlement::ObservationUnavailable {
                message: "ACP terminal observation connection closed after write".to_string(),
            }
        } else {
            AcpTerminalObservationSettlement::FailedBeforeWrite {
                message: "ACP terminal observation connection closed before write".to_string(),
            }
        };
        let _ = route.client.settle_acp_terminal_observation(
            route.call_id,
            route.capability_revision,
            settlement,
        );
    }
}

fn retire_all_terminal_cleanup_routes(routes: &TerminalCleanupRoutes) {
    let retired = routes
        .pending
        .lock()
        .expect("ACP terminal cleanup route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in retired {
        let _ = route.client.settle_acp_terminal_cleanup(
            route.call_id,
            route.capability_revision,
            AcpTerminalCleanupSettlement::ExternalEffectAmbiguous {
                message: "ACP terminal cleanup connection closed before response".to_string(),
            },
        );
    }
}

async fn enqueue_json(facade: &RpcFacadeHandle, value: Value) -> Result<(), RpcFacadeError> {
    let mut encoded = serde_json::to_vec(&value).map_err(|error| RpcFacadeError::Protocol {
        message: error.to_string(),
    })?;
    encoded.push(b'\n');
    facade
        .enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded))?
        .ack()
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::Instant;

    use agent_client_protocol::RequestPermissionOutcome;
    use agent_client_protocol::{
        CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities,
        Implementation, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    };
    use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalRequest};
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::RawToolCall;
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
    use orca_mcp::{McpElicitationMode, McpElicitationRequest, McpElicitationResponse};
    use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

    use super::*;
    use crate::lifecycle::RuntimeUserInputRequest;
    use crate::model_response::RuntimeModelResponse;
    use crate::protocol::{
        PermissionResponseDecision, RequestFileSystemPermissions, RequestPermissionProfile,
    };
    use crate::runtime_host::{
        GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
        ThreadOperationOutcome,
    };
    use crate::runtime_permission::RuntimePermissionRequest;
    use crate::thread::RuntimeThread;

    #[cfg(windows)]
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);
    #[cfg(not(windows))]
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn test_absolute_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    struct WaitForCancelExecutor;

    struct CompleteWithMessageExecutor;

    struct ReadTextFileExecutor {
        content_tx: std::sync::mpsc::SyncSender<String>,
    }

    struct WriteTextFileExecutor {
        outcome_tx: std::sync::mpsc::SyncSender<Result<(), io::ErrorKind>>,
    }

    struct TerminalCreateExecutor {
        outcome_tx: std::sync::mpsc::SyncSender<Result<String, io::ErrorKind>>,
        created_tx: Option<std::sync::mpsc::SyncSender<()>>,
        continue_rx: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
    }

    struct TerminalObserveExecutor {
        outcome_tx:
            std::sync::mpsc::SyncSender<Result<(String, bool, Option<u32>, u32), io::ErrorKind>>,
    }

    struct TerminalWaitBoundaryExecutor {
        outcome_tx: std::sync::mpsc::SyncSender<Result<usize, io::ErrorKind>>,
    }

    struct StandardInteractionExecutor {
        behaviors: Mutex<Vec<StandardInteractionBehavior>>,
        outcome_tx: std::sync::mpsc::SyncSender<StandardInteractionOutcome>,
    }

    #[derive(Clone, Copy)]
    enum StandardInteractionBehavior {
        ToolApproval,
        PermissionRequest,
        UserInput,
        McpElicitation,
    }

    #[derive(Debug, PartialEq)]
    enum StandardInteractionOutcome {
        ToolApproval(ApprovalDecision),
        PermissionRequest(PermissionResponseDecision),
        UserInput(Option<String>),
        McpElicitation(McpElicitationResponse),
    }

    struct TerminalOutputCancelExecutor;

    struct MultiTerminalCreateExecutor {
        outcome_tx: std::sync::mpsc::SyncSender<Result<Vec<String>, io::ErrorKind>>,
        created_tx: Option<std::sync::mpsc::SyncSender<()>>,
        continue_rx: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
    }

    #[derive(Default)]
    struct FlushFailureSignal {
        fail: AtomicBool,
        waker: Mutex<Option<Waker>>,
    }

    impl FlushFailureSignal {
        fn fail(&self) {
            self.fail.store(true, Ordering::Release);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }
    }

    struct FailWriteTextFileFlush<W> {
        inner: W,
        signal: Arc<FlushFailureSignal>,
        method: &'static [u8],
        fail_current_flush: bool,
    }

    impl<W> FailWriteTextFileFlush<W> {
        fn new(inner: W, signal: Arc<FlushFailureSignal>) -> Self {
            Self {
                inner,
                signal,
                method: b"fs/write_text_file",
                fail_current_flush: false,
            }
        }

        fn for_terminal_cleanup(inner: W, signal: Arc<FlushFailureSignal>) -> Self {
            Self {
                inner,
                signal,
                method: b"terminal/kill",
                fail_current_flush: false,
            }
        }
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for FailWriteTextFileFlush<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if bytes
                .windows(self.method.len())
                .any(|window| window == self.method)
            {
                self.fail_current_flush = true;
            }
            Pin::new(&mut self.inner).poll_write(cx, bytes)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.fail_current_flush {
                if self.signal.fail.load(Ordering::Acquire) {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected write-text-file flush failure",
                    )));
                }
                *self.signal.waker.lock().unwrap() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    impl ThreadOperationExecutor for CompleteWithMessageExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: Vec::new(),
                    assistant_content: Some("typed update".to_string()),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for ReadTextFileExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let path = test_absolute_path("orca-acp-notes.txt");
            let tool = ToolRequest {
                id: "read-capability-1".to_string(),
                name: ToolName::ReadFile,
                action: orca_core::approval_types::ActionKind::Read,
                target: Some(path.display().to_string()),
                raw_arguments: Some(json!({ "path": path, "line": 2, "limit": 3 }).to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let content =
                match generation.read_text_file_from_acp_client(&tool, path, Some(2), Some(3)) {
                    Ok(content) => content,
                    Err(_) if cancel.is_cancelled() => return Ok(RunStatus::Cancelled.into()),
                    Err(error) => return Err(error),
                };
            self.content_tx.send(content).unwrap();
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                "read through ACP client".to_string(),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for WriteTextFileExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let path = test_absolute_path("orca-acp-output.txt");
            let tool = ToolRequest {
                id: "write-capability-1".to_string(),
                name: ToolName::WriteFile,
                action: orca_core::approval_types::ActionKind::Write,
                target: Some(path.display().to_string()),
                raw_arguments: Some(
                    json!({ "path": path, "content": "written by Orca\n" }).to_string(),
                ),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let outcome = generation.write_text_file_to_acp_client(
                &tool,
                path,
                "written by Orca\n".to_string(),
            );
            self.outcome_tx
                .send(outcome.as_ref().map(|_| ()).map_err(io::Error::kind))
                .unwrap();
            outcome?;
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                "wrote through ACP client".to_string(),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for TerminalCreateExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "terminal-create-1".to_string(),
                name: ToolName::Bash,
                action: orca_core::approval_types::ActionKind::Shell,
                target: Some("printf".to_string()),
                raw_arguments: Some(r#"{"command":"printf","args":["hello"]}"#.to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let outcome = generation.create_terminal_on_acp_client(
                &tool,
                "printf".to_string(),
                vec!["hello".to_string()],
                vec![("LANG".to_string(), "C".to_string())],
                Some(test_absolute_path("orca-acp-workspace")),
                Some(4096),
            );
            let terminal = match outcome {
                Ok(terminal) => terminal,
                Err(error) => {
                    self.outcome_tx.send(Err(error.kind())).unwrap();
                    return Err(error);
                }
            };
            let terminal_id = terminal.terminal_id().to_string();
            if let Some(created_tx) = &self.created_tx {
                created_tx.send(()).unwrap();
            }
            if let Some(continue_rx) = &self.continue_rx {
                continue_rx.lock().unwrap().recv().unwrap();
            }
            let cleanup = terminal.close();
            self.outcome_tx
                .send(
                    cleanup
                        .as_ref()
                        .map(|_| terminal_id.clone())
                        .map_err(io::Error::kind),
                )
                .unwrap();
            cleanup?;
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                format!("created terminal {terminal_id}"),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for TerminalObserveExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "terminal-observe-1".to_string(),
                name: ToolName::Bash,
                action: orca_core::approval_types::ActionKind::Shell,
                target: Some("printf".to_string()),
                raw_arguments: Some(r#"{"command":"printf","args":["hello"]}"#.to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let terminal = generation.create_terminal_on_acp_client(
                &tool,
                "printf".to_string(),
                vec!["hello".to_string()],
                Vec::new(),
                Some(test_absolute_path("orca-acp-workspace")),
                Some(4096),
            )?;
            let output = terminal.output()?;
            let exit = terminal.wait_for_exit()?;
            let observed = (
                output.output().to_string(),
                output.truncated(),
                output.exit_status().and_then(|status| status.exit_code()),
                exit.exit_code().expect("test terminal exits with a code"),
            );
            terminal.close()?;
            self.outcome_tx.send(Ok(observed)).unwrap();
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                "observed terminal through ACP client".to_string(),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for TerminalWaitBoundaryExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "terminal-wait-boundary".to_string(),
                name: ToolName::Bash,
                action: orca_core::approval_types::ActionKind::Shell,
                target: Some("wait".to_string()),
                raw_arguments: Some(r#"{"command":"wait"}"#.to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let terminal = generation.create_terminal_on_acp_client(
                &tool,
                "wait".to_string(),
                Vec::new(),
                Vec::new(),
                Some(test_absolute_path("orca-acp-workspace")),
                None,
            )?;
            let wait = terminal
                .wait_for_exit()
                .map(|status| status.signal().map(str::len).unwrap_or_default());
            let cleanup = terminal.close();
            self.outcome_tx
                .send(wait.as_ref().copied().map_err(io::Error::kind))
                .unwrap();
            let signal_len = wait?;
            cleanup?;
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                format!("observed terminal signal with {signal_len} bytes"),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for StandardInteractionExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let behavior = self.behaviors.lock().unwrap().remove(0);
            let tool = ToolRequest {
                id: "standard-interaction-tool".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf".to_string()),
                raw_arguments: Some(r#"{"command":"printf","args":["hello"]}"#.to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let outcome = match behavior {
                StandardInteractionBehavior::ToolApproval
                | StandardInteractionBehavior::PermissionRequest => {
                    turn_request
                        .provider_response_ingress()
                        .expect("typed ACP operation provides response ingress")
                        .commit_response(&RuntimeModelResponse::new(
                            ProviderResponse {
                                steps: vec![ProviderStep::ToolCall(tool.clone())],
                                assistant_content: None,
                                assistant_reasoning: None,
                                tool_calls: vec![RawToolCall {
                                    id: tool.id.clone(),
                                    function_name: tool.name.as_str().to_string(),
                                    arguments: tool.raw_arguments.clone().unwrap(),
                                }],
                                usage: None,
                            },
                            request.turn_id().clone(),
                        ))?;
                    match behavior {
                        StandardInteractionBehavior::ToolApproval => {
                            let approval = turn_request
                                .approval_handler()
                                .expect("typed ACP operation provides approval broker")
                                .resolve_interactive(
                                    &ApprovalRequest {
                                        id: "standard-tool-approval".to_string(),
                                        action: ActionKind::Shell,
                                        description: "run exact standard tool".to_string(),
                                        tool: Some(tool.name.as_str().to_string()),
                                        target: tool.target.clone(),
                                        preview: None,
                                    },
                                    &tool,
                                )?;
                            StandardInteractionOutcome::ToolApproval(approval.decision)
                        }
                        StandardInteractionBehavior::PermissionRequest => {
                            let permission = turn_request
                                .permission_handler()
                                .expect("typed ACP operation provides permission broker")
                                .request_permissions(&RuntimePermissionRequest {
                                    id: tool.id.clone(),
                                    reason: Some("write generated output".to_string()),
                                    permissions: RequestPermissionProfile {
                                        file_system: Some(RequestFileSystemPermissions {
                                            read: None,
                                            write: Some(vec![test_absolute_path(
                                                "orca-acp-output",
                                            )]),
                                            entries: None,
                                        }),
                                        network: None,
                                        shell: None,
                                    },
                                })?;
                            StandardInteractionOutcome::PermissionRequest(permission.decision)
                        }
                        _ => unreachable!(),
                    }
                }
                StandardInteractionBehavior::UserInput => StandardInteractionOutcome::UserInput(
                    generation
                        .user_input_handler()
                        .expect("typed ACP operation provides user-input broker")
                        .request_user_input(&RuntimeUserInputRequest {
                            id: "standard-user-input".to_string(),
                            question: "Continue?".to_string(),
                            choices: vec!["yes".to_string(), "no".to_string()],
                        })?,
                ),
                StandardInteractionBehavior::McpElicitation => {
                    StandardInteractionOutcome::McpElicitation(
                        generation
                            .mcp_elicitation_handler()
                            .expect("typed ACP operation provides MCP elicitation broker")
                            .handle_elicitation(McpElicitationRequest {
                                server_name: "docs".to_string(),
                                id: "standard-mcp-elicitation".to_string(),
                                mode: McpElicitationMode::Url,
                                message: "Open sign-in?".to_string(),
                                url: Some("https://example.com/sign-in".to_string()),
                                requested_schema: None,
                            })
                            .map_err(io::Error::other)?,
                    )
                }
            };
            self.outcome_tx.send(outcome).unwrap();
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for TerminalOutputCancelExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "terminal-output-cancel".to_string(),
                name: ToolName::Bash,
                action: orca_core::approval_types::ActionKind::Shell,
                target: Some("sleep".to_string()),
                raw_arguments: Some(r#"{"command":"sleep","args":["30"]}"#.to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let terminal = generation.create_terminal_on_acp_client(
                &tool,
                "sleep".to_string(),
                vec!["30".to_string()],
                Vec::new(),
                Some(test_absolute_path("orca-acp-workspace")),
                Some(4096),
            )?;
            match terminal.output() {
                Err(_) if cancel.is_cancelled() => Ok(RunStatus::Cancelled.into()),
                Err(error) => Err(error),
                Ok(_) => Err(io::Error::other(
                    "terminal output unexpectedly completed before cancellation",
                )),
            }
        }
    }

    impl ThreadOperationExecutor for MultiTerminalCreateExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "terminal-create-shared-tool".to_string(),
                name: ToolName::Bash,
                action: orca_core::approval_types::ActionKind::Shell,
                target: Some("printf".to_string()),
                raw_arguments: Some(r#"{"command":"printf","args":["hello"]}"#.to_string()),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let first = generation.create_terminal_on_acp_client(
                &tool,
                "printf".to_string(),
                vec!["first".to_string()],
                Vec::new(),
                Some(test_absolute_path("orca-acp-workspace")),
                Some(4096),
            )?;
            let second = generation.create_terminal_on_acp_client(
                &tool,
                "printf".to_string(),
                vec!["second".to_string()],
                Vec::new(),
                Some(test_absolute_path("orca-acp-workspace")),
                Some(4096),
            )?;
            let terminal_ids = vec![
                first.terminal_id().to_string(),
                second.terminal_id().to_string(),
            ];
            if let Some(created_tx) = &self.created_tx {
                created_tx.send(()).unwrap();
            }
            if let Some(continue_rx) = &self.continue_rx {
                continue_rx.lock().unwrap().recv().unwrap();
            }
            let (first_result, second_result) = std::thread::scope(|scope| {
                let first_cleanup = scope.spawn(move || first.close());
                let second_cleanup = scope.spawn(move || second.close());
                (
                    first_cleanup.join().expect("first cleanup thread"),
                    second_cleanup.join().expect("second cleanup thread"),
                )
            });
            let cleanup = first_result.and(second_result);
            self.outcome_tx
                .send(
                    cleanup
                        .as_ref()
                        .map(|_| terminal_ids.clone())
                        .map_err(io::Error::kind),
                )
                .unwrap();
            cleanup?;
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                "created and released two terminals".to_string(),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for WaitForCancelExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let deadline = Instant::now() + TEST_TIMEOUT;
            while !cancel.is_cancelled() {
                assert!(
                    Instant::now() < deadline,
                    "ACP cancel did not reach runtime"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(RunStatus::Cancelled.into())
        }
    }

    #[test]
    fn bounded_production_connection_binds_prompt_before_later_wire_cancel() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host = RuntimeHost::start_with_executor(Arc::new(WaitForCancelExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0")),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("wait".to_string())],
                ),
            )
            .await;
            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "cancelled");
            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_flushes_typed_updates_before_prompt_response() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host =
                RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0")),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("complete".to_string())],
                ),
            )
            .await;

            let first = read_value(&mut client_read).await;
            assert_eq!(first["method"], "session/update");
            assert_eq!(first["params"]["update"]["content"]["text"], "typed update");
            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_routes_only_standard_tool_approval_and_fails_extensions_closed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(StandardInteractionExecutor {
                behaviors: Mutex::new(vec![
                    StandardInteractionBehavior::ToolApproval,
                    StandardInteractionBehavior::PermissionRequest,
                    StandardInteractionBehavior::UserInput,
                    StandardInteractionBehavior::McpElicitation,
                ]),
                outcome_tx,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0")),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("standard tool approval".to_string())],
                ),
            )
            .await;
            let mut permission_requests = 0;
            let approval_prompt = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "session/request_permission" {
                    permission_requests += 1;
                    assert_eq!(
                        value["params"]["options"]
                            .as_array()
                            .expect("permission options")
                            .len(),
                        2
                    );
                    write_raw_response(
                        &mut client_write,
                        value["id"].as_i64().expect("permission request id"),
                        serde_json::to_value(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                        .unwrap(),
                    )
                    .await;
                } else if value.get("id").and_then(Value::as_i64) == Some(3) {
                    break value;
                }
            };
            assert_eq!(approval_prompt["result"]["stopReason"], "end_turn");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                StandardInteractionOutcome::ToolApproval(ApprovalDecision::Deny)
            );

            for (index, (request_id, prompt, expected)) in [
                (
                    4,
                    "extensionless permission request",
                    StandardInteractionOutcome::PermissionRequest(PermissionResponseDecision::Deny),
                ),
                (
                    5,
                    "extensionless user input",
                    StandardInteractionOutcome::UserInput(None),
                ),
                (
                    6,
                    "extensionless MCP elicitation",
                    StandardInteractionOutcome::McpElicitation(McpElicitationResponse::Decline),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let session_request_id = 20 + index as i64;
                write_request(
                    &mut client_write,
                    session_request_id,
                    "session/new",
                    NewSessionRequest::new(cwd.path().to_path_buf()),
                )
                .await;
                let new_session = read_response(&mut client_read, session_request_id).await;
                let interaction_session_id = new_session["result"]["sessionId"]
                    .as_str()
                    .expect("interaction session id")
                    .to_string();
                write_request(
                    &mut client_write,
                    request_id,
                    "session/prompt",
                    PromptRequest::new(
                        SessionId::new(interaction_session_id),
                        vec![ContentBlock::from(prompt.to_string())],
                    ),
                )
                .await;
                let response = loop {
                    let value = read_value(&mut client_read).await;
                    assert_ne!(
                        value["method"], "session/request_permission",
                        "extension-only interaction reached standard ACP wire: {prompt}"
                    );
                    if value.get("id").and_then(Value::as_i64) == Some(request_id) {
                        break value;
                    }
                };
                assert!(
                    response.get("error").is_some(),
                    "unexpected prompt: {response}"
                );
                assert_eq!(
                    outcome_rx
                        .recv_timeout(TEST_TIMEOUT)
                        .unwrap_or_else(|error| panic!("missing {prompt} outcome: {error:?}")),
                    expected
                );
            }
            assert_eq!(permission_requests, 1);

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_routes_read_text_file_through_runtime_owned_capability_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (content_tx, content_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(ReadTextFileExecutor { content_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().read_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("read notes".to_string())],
                ),
            )
            .await;

            let read_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/read_text_file" {
                    break value;
                }
            };
            assert_eq!(read_request["params"]["sessionId"], session_id);
            assert_eq!(
                read_request["params"]["path"],
                test_absolute_path("orca-acp-notes.txt")
                    .display()
                    .to_string()
            );
            assert_eq!(read_request["params"]["line"], 2);
            assert_eq!(read_request["params"]["limit"], 3);
            let written_before_response = persisted_capability_is_written(
                &transcript_path,
                crate::surface::SurfaceCapabilityCallKind::ReadTextFile,
            );
            let read_id = read_request["id"].as_i64().expect("reverse request id");
            write_raw_response(
                &mut client_write,
                read_id,
                json!({ "content": "line two\nline three\nline four\n" }),
            )
            .await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(
                content_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                "line two\nline three\nline four\n"
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
            assert!(
                written_before_response,
                "read request reached the client before WrittenAwaitingResponse was durable"
            );
        });
    }

    #[test]
    fn production_connection_routes_write_text_file_after_runtime_delivery_barrier() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;

            let write_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break value;
                }
            };
            assert_eq!(write_request["params"]["sessionId"], session_id);
            assert_eq!(
                write_request["params"]["path"],
                test_absolute_path("orca-acp-output.txt")
                    .display()
                    .to_string()
            );
            assert_eq!(write_request["params"]["content"], "written by Orca\n");
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool waiter completed before the physical response"
            );
            let write_id = write_request["id"].as_i64().expect("reverse request id");
            write_raw_response(&mut client_write, write_id, json!({})).await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(), Ok(()));

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_releases_runtime_owned_terminal_before_tool_resumes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;

            let create_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            assert_eq!(create_request["params"]["sessionId"], session_id);
            assert_eq!(create_request["params"]["command"], "printf");
            assert_eq!(create_request["params"]["args"], json!(["hello"]));
            assert_eq!(
                create_request["params"]["cwd"],
                test_absolute_path("orca-acp-workspace")
                    .display()
                    .to_string()
            );
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool waiter completed before the terminal identity was durable"
            );
            let request_id = create_request["id"]
                .as_i64()
                .expect("terminal reverse request id");
            write_raw_response(
                &mut client_write,
                request_id,
                json!({"terminalId":"terminal-1"}),
            )
            .await;

            let kill_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/kill" {
                    break value;
                }
            };
            assert_eq!(kill_request["params"]["terminalId"], "terminal-1");
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool resumed before terminal kill settled"
            );
            let kill_id = kill_request["id"].as_i64().expect("terminal kill id");
            write_raw_response(&mut client_write, kill_id, json!({})).await;

            let release_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/release" {
                    break value;
                }
            };
            assert_eq!(release_request["params"]["terminalId"], "terminal-1");
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool resumed before terminal release settled"
            );
            let release_id = release_request["id"].as_i64().expect("terminal release id");
            write_raw_response(&mut client_write, release_id, json!({})).await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Ok("terminal-1".to_string())
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
            assert_persisted_terminal_released(&transcript_path, "terminal-1");
        });
    }

    #[test]
    fn production_connection_observes_terminal_output_and_exit_before_release() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(TerminalObserveExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("observe terminal".to_string())],
                ),
            )
            .await;

            let create_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            write_raw_response(
                &mut client_write,
                create_request["id"].as_i64().expect("terminal create id"),
                json!({"terminalId":"terminal-observe"}),
            )
            .await;

            let output_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/output" {
                    break value;
                }
            };
            assert_eq!(output_request["params"]["terminalId"], "terminal-observe");
            assert_persisted_capability_written(
                &transcript_path,
                crate::surface::SurfaceCapabilityCallKind::TerminalOutput,
            );
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool resumed before terminal output settled"
            );
            write_raw_response(
                &mut client_write,
                output_request["id"].as_i64().expect("terminal output id"),
                json!({
                    "output":"hello",
                    "truncated":false,
                    "exitStatus":{"exitCode":0,"signal":null}
                }),
            )
            .await;

            let wait_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/wait_for_exit" {
                    break value;
                }
            };
            assert_eq!(wait_request["params"]["terminalId"], "terminal-observe");
            assert_persisted_capability_written(
                &transcript_path,
                crate::surface::SurfaceCapabilityCallKind::TerminalWaitForExit,
            );
            assert_persisted_terminal_observation(
                &transcript_path,
                crate::surface::SurfaceCapabilityCallKind::TerminalOutput,
            );
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool resumed before terminal exit settled"
            );
            write_raw_response(
                &mut client_write,
                wait_request["id"].as_i64().expect("terminal wait id"),
                json!({"exitCode":0,"signal":null}),
            )
            .await;

            let kill_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/kill" {
                    break value;
                }
            };
            assert_persisted_terminal_observation(
                &transcript_path,
                crate::surface::SurfaceCapabilityCallKind::TerminalWaitForExit,
            );
            write_raw_response(
                &mut client_write,
                kill_request["id"].as_i64().expect("terminal kill id"),
                json!({}),
            )
            .await;
            let release_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/release" {
                    break value;
                }
            };
            write_raw_response(
                &mut client_write,
                release_request["id"].as_i64().expect("terminal release id"),
                json!({}),
            )
            .await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Ok(("hello".to_string(), false, Some(0), 0))
            );
            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
            assert_persisted_terminal_released(&transcript_path, "terminal-observe");
        });
    }

    #[test]
    fn production_connection_enforces_terminal_wait_canonical_result_limit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let at_limit = terminal_wait_signal_for_canonical_len(
                crate::surface::ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT as usize,
            );
            let over_limit = format!("{at_limit}x");
            run_terminal_wait_boundary_case(at_limit, true).await;
            run_terminal_wait_boundary_case(over_limit, false).await;
        });
    }

    async fn run_terminal_wait_boundary_case(signal: String, expect_completed: bool) {
        let expected_signal_len = signal.len();
        let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(TerminalWaitBoundaryExecutor { outcome_tx }))
                .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_read, mut client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let connection = tokio::task::spawn_local(run_connection(
            host.surface_handle(),
            test_config(cwd.path().to_path_buf()),
            server_read,
            server_write,
        ));
        let mut client_read = BufReader::new(client_read);

        write_request(
            &mut client_write,
            1,
            "initialize",
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(Implementation::new("bounded-test", "0.0.0"))
                .client_capabilities(ClientCapabilities::new().terminal(true)),
        )
        .await;
        let _ = read_response(&mut client_read, 1).await;
        write_request(
            &mut client_write,
            2,
            "session/new",
            NewSessionRequest::new(cwd.path().to_path_buf()),
        )
        .await;
        let new_session = read_response(&mut client_read, 2).await;
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();
        let transcript_path = crate::thread_store::find_session_path(&session_id, true)
            .unwrap()
            .expect("recording ACP session path");
        write_request(
            &mut client_write,
            3,
            "session/prompt",
            PromptRequest::new(
                SessionId::new(session_id),
                vec![ContentBlock::from("wait for terminal boundary".to_string())],
            ),
        )
        .await;

        let create_request = loop {
            let value = read_value(&mut client_read).await;
            if value["method"] == "terminal/create" {
                break value;
            }
        };
        write_raw_response(
            &mut client_write,
            create_request["id"].as_i64().expect("terminal create id"),
            json!({"terminalId":"terminal-wait-boundary"}),
        )
        .await;
        let wait_request = loop {
            let value = read_value(&mut client_read).await;
            if value["method"] == "terminal/wait_for_exit" {
                break value;
            }
        };
        write_raw_response(
            &mut client_write,
            wait_request["id"].as_i64().expect("terminal wait id"),
            json!({"exitCode":0,"signal":signal}),
        )
        .await;
        let kill_request = loop {
            let value = read_value(&mut client_read).await;
            if value["method"] == "terminal/kill" {
                break value;
            }
        };
        write_raw_response(
            &mut client_write,
            kill_request["id"].as_i64().expect("terminal kill id"),
            json!({}),
        )
        .await;
        let release_request = loop {
            let value = read_value(&mut client_read).await;
            if value["method"] == "terminal/release" {
                break value;
            }
        };
        write_raw_response(
            &mut client_write,
            release_request["id"].as_i64().expect("terminal release id"),
            json!({}),
        )
        .await;

        let prompt = read_response(&mut client_read, 3).await;
        let outcome = outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        if expect_completed {
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(outcome, Ok(expected_signal_len));
        } else {
            assert!(prompt.get("error").is_some(), "unexpected prompt: {prompt}");
            assert_eq!(outcome, Err(io::ErrorKind::InvalidData));
        }

        client_write.shutdown().await.unwrap();
        tokio::time::timeout(TEST_TIMEOUT, connection)
            .await
            .expect("connection shutdown")
            .expect("connection task")
            .expect("clean connection");
        host.shutdown().unwrap();
        assert_persisted_terminal_wait_limit_state(&transcript_path, expect_completed);
    }

    fn terminal_wait_signal_for_canonical_len(target: usize) -> String {
        let sample = crate::surface::CapabilityCallResult::TerminalExitObserved {
            exit_status: crate::surface::SurfaceTerminalExitStatus {
                exit_code: Some(0),
                signal: Some(crate::surface::NonEmptyText::try_new("x").unwrap()),
            },
        };
        let sample_len = serde_json::to_vec(&sample).unwrap().len();
        let signal_len = target
            .checked_sub(sample_len - 1)
            .expect("canonical capability target exceeds fixed encoding overhead");
        let signal = "x".repeat(signal_len);
        let result = crate::surface::CapabilityCallResult::TerminalExitObserved {
            exit_status: crate::surface::SurfaceTerminalExitStatus {
                exit_code: Some(0),
                signal: Some(crate::surface::NonEmptyText::try_new(signal.clone()).unwrap()),
            },
        };
        assert_eq!(serde_json::to_vec(&result).unwrap().len(), target);
        signal
    }

    #[test]
    fn concurrent_terminals_from_one_tool_keep_cleanup_identity_exact() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(MultiTerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create two terminals".to_string())],
                ),
            )
            .await;

            for terminal_id in ["terminal-a", "terminal-b"] {
                let create_request = loop {
                    let value = read_value(&mut client_read).await;
                    if value["method"] == "terminal/create" {
                        break value;
                    }
                };
                write_raw_response(
                    &mut client_write,
                    create_request["id"].as_i64().expect("terminal create id"),
                    json!({"terminalId": terminal_id}),
                )
                .await;
            }

            let mut cleanups = Vec::new();
            for _ in 0..3 {
                let cleanup_request = loop {
                    let value = read_value(&mut client_read).await;
                    if matches!(
                        value["method"].as_str(),
                        Some("terminal/kill" | "terminal/release")
                    ) {
                        break value;
                    }
                };
                let method = cleanup_request["method"]
                    .as_str()
                    .expect("cleanup method")
                    .to_string();
                let terminal_id = cleanup_request["params"]["terminalId"]
                    .as_str()
                    .expect("cleanup terminal id")
                    .to_string();
                cleanups.push((method, terminal_id));
                write_raw_response(
                    &mut client_write,
                    cleanup_request["id"].as_i64().expect("terminal cleanup id"),
                    json!({}),
                )
                .await;
            }
            let killed_terminals = cleanups
                .iter()
                .filter(|(method, _)| method == "terminal/kill")
                .map(|(_, terminal_id)| terminal_id.as_str())
                .collect::<BTreeSet<_>>();
            let released_terminals = cleanups
                .iter()
                .filter(|(method, _)| method == "terminal/release")
                .map(|(_, terminal_id)| terminal_id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                killed_terminals,
                BTreeSet::from(["terminal-a", "terminal-b"]),
                "both exact terminal identities must be killed: {cleanups:?}"
            );
            assert_eq!(
                released_terminals.len(),
                1,
                "exactly one terminal release must complete: {cleanups:?}"
            );
            let released_terminal = released_terminals[0];
            assert!(
                killed_terminals.contains(released_terminal),
                "release must preserve one of the exact killed terminal identities: {cleanups:?}"
            );
            let unresolved_terminal = killed_terminals
                .iter()
                .find_map(|terminal_id| {
                    (*terminal_id != released_terminal).then_some((*terminal_id).to_string())
                })
                .expect("one killed terminal remains unreleased");
            assert_eq!(
                tokio::task::spawn_blocking(move || outcome_rx.recv_timeout(TEST_TIMEOUT))
                    .await
                    .expect("multi-terminal outcome task")
                    .expect("multi-terminal outcome"),
                Err(io::ErrorKind::Other)
            );
            assert_persisted_terminal_cleanup_ambiguous(
                &transcript_path,
                crate::surface::ExternalEffectKind::TerminalRelease,
                &unresolved_terminal,
            );
            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn host_shutdown_settles_two_resident_cleanup_calls_before_one_tool_completion() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(MultiTerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create two terminals".to_string())],
                ),
            )
            .await;
            for terminal_id in ["terminal-resident-a", "terminal-resident-b"] {
                let create_request = loop {
                    let value = read_value(&mut client_read).await;
                    if value["method"] == "terminal/create" {
                        break value;
                    }
                };
                write_raw_response(
                    &mut client_write,
                    create_request["id"].as_i64().expect("terminal create id"),
                    json!({"terminalId": terminal_id}),
                )
                .await;
            }
            let first_cleanup = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/kill" {
                    break value;
                }
            };
            assert!(matches!(
                first_cleanup["params"]["terminalId"].as_str(),
                Some("terminal-resident-a" | "terminal-resident-b")
            ));

            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    let resident_cleanup_calls = persisted_surface_events(&transcript_path)
                        .into_iter()
                        .filter_map(|event| match event {
                            crate::surface::SurfaceEvent::Tool(
                                crate::surface::ToolPatch::CapabilityCallChanged {
                                    call:
                                        crate::surface::SurfaceCapabilityCall {
                                            call_id,
                                            kind:
                                                crate::surface::SurfaceCapabilityCallKind::TerminalKill,
                                            state:
                                                crate::surface::SurfaceCapabilityCallState::Prepared
                                                | crate::surface::SurfaceCapabilityCallState::DeliveryPossible
                                                | crate::surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                                            ..
                                        },
                                },
                            ) => Some(call_id),
                            _ => None,
                        })
                        .collect::<BTreeSet<_>>();
                    if resident_cleanup_calls.len() == 2 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("both cleanup calls became runtime-resident");

            let mut shutdown = tokio::task::spawn_blocking(move || host.shutdown());
            let shutdown_result = tokio::time::timeout(TEST_TIMEOUT, &mut shutdown).await;
            if shutdown_result.is_err() {
                let _ = client_write.shutdown().await;
                drop(client_read);
                let _ = tokio::time::timeout(TEST_TIMEOUT, &mut shutdown).await;
                let diagnostics = persisted_surface_events(&transcript_path)
                    .into_iter()
                    .filter_map(|event| match event {
                        crate::surface::SurfaceEvent::Tool(
                            crate::surface::ToolPatch::CapabilityCallChanged { call },
                        ) if matches!(
                            call.kind,
                            crate::surface::SurfaceCapabilityCallKind::TerminalKill
                                | crate::surface::SurfaceCapabilityCallKind::TerminalRelease
                        ) =>
                        {
                            Some(format!("call:{:?}:{:?}", call.call_id, call.state))
                        }
                        crate::surface::SurfaceEvent::Tool(
                            crate::surface::ToolPatch::RemoteTerminalLeaseChanged { lease },
                        ) => Some(format!("lease:{:?}:{:?}", lease.lease_id, lease.state)),
                        crate::surface::SurfaceEvent::Operation(
                            crate::surface::OperationPatch::ControlIntentCommitted { .. },
                        ) => Some("control-intent".to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                panic!(
                    "host shutdown did not settle both resident cleanup calls: {diagnostics:#?}"
                );
            }
            shutdown_result
                .expect("host shutdown timeout")
                .expect("host shutdown task")
                .expect("host shutdown");
            assert!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap().is_err(),
                "shutdown must not resume the shared tool as successful"
            );

            let events = persisted_surface_events(&transcript_path);
            let ambiguous_terminal_ids = events
                .iter()
                .filter_map(|event| match event {
                    crate::surface::SurfaceEvent::Tool(
                        crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                            lease:
                                crate::surface::SurfaceRemoteTerminalLease {
                                    state:
                                        crate::surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                            terminal_id: Some(terminal_id),
                                            ..
                                        },
                                    ..
                                },
                        },
                    ) => Some(terminal_id.as_str().to_string()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            assert!(ambiguous_terminal_ids.contains("terminal-resident-a"));
            assert!(ambiguous_terminal_ids.contains("terminal-resident-b"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        crate::surface::SurfaceEvent::Tool(
                            crate::surface::ToolPatch::Completed {
                                result:
                                    crate::surface::SurfaceToolResult {
                                        tool_call_id,
                                        ..
                                    },
                            },
                        ) if tool_call_id.as_str() == "terminal-create-shared-tool"
                    ))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        crate::surface::SurfaceEvent::Item(
                            crate::surface::ItemPatch::Added {
                                item:
                                    crate::surface::SurfaceItem::ToolResultMessage {
                                        tool_call_id,
                                        ..
                                    },
                            },
                        ) if tool_call_id.as_str() == "terminal-create-shared-tool"
                    ))
                    .count(),
                1
            );

            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
        });
    }

    #[test]
    fn terminal_kill_connection_loss_persists_cleanup_ambiguity() {
        terminal_cleanup_connection_loss_is_durable(false);
    }

    #[test]
    fn terminal_release_connection_loss_persists_cleanup_ambiguity() {
        terminal_cleanup_connection_loss_is_durable(true);
    }

    #[test]
    fn host_shutdown_terminalizes_live_terminal_cleanup_as_ambiguous() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);
            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;
            let create_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            let create_id = create_request["id"].as_i64().expect("terminal create id");
            write_raw_response(
                &mut client_write,
                create_id,
                json!({"terminalId":"terminal-shutdown"}),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/kill" {
                    break;
                }
            }

            tokio::task::spawn_blocking(move || host.shutdown())
                .await
                .expect("host shutdown task")
                .expect("host shutdown");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::Interrupted)
            );
            assert_persisted_terminal_cleanup_ambiguous(
                &transcript_path,
                crate::surface::ExternalEffectKind::TerminalKill,
                "terminal-shutdown",
            );
            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
        });
    }

    #[test]
    fn host_shutdown_terminalizes_live_lease_before_cleanup_admission() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
            let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                outcome_tx,
                created_tx: Some(created_tx),
                continue_rx: Some(Mutex::new(continue_rx)),
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);
            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;
            let create_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            write_raw_response(
                &mut client_write,
                create_request["id"].as_i64().expect("terminal create id"),
                json!({"terminalId":"terminal-live-shutdown"}),
            )
            .await;
            tokio::task::spawn_blocking(move || created_rx.recv_timeout(TEST_TIMEOUT))
                .await
                .expect("created barrier task")
                .expect("executor observed durable live lease");

            let shutdown = tokio::task::spawn_blocking(move || host.shutdown());
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    if persisted_surface_events(&transcript_path)
                        .iter()
                        .any(|event| matches!(
                            event,
                            crate::surface::SurfaceEvent::Tool(
                                crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                                    lease:
                                        crate::surface::SurfaceRemoteTerminalLease {
                                            state:
                                                crate::surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                                    terminal_id: Some(terminal_id),
                                                    ..
                                                },
                                            ..
                                        },
                                },
                            ) if terminal_id.as_str() == "terminal-live-shutdown"
                        ))
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("shutdown persisted live lease terminalization");
            continue_tx.send(()).unwrap();
            shutdown
                .await
                .expect("host shutdown task")
                .expect("host shutdown");
            assert!(matches!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe)
            ));
            assert_persisted_terminal_cleanup_ambiguous(
                &transcript_path,
                crate::surface::ExternalEffectKind::TerminalKill,
                "terminal-live-shutdown",
            );
            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
        });
    }

    #[test]
    fn host_shutdown_terminalizes_two_live_leases_with_one_tool_completion() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
            let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(MultiTerminalCreateExecutor {
                outcome_tx,
                created_tx: Some(created_tx),
                continue_rx: Some(Mutex::new(continue_rx)),
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);
            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create two terminals".to_string())],
                ),
            )
            .await;

            for terminal_id in ["terminal-live-a", "terminal-live-b"] {
                let create_request = loop {
                    let value = read_value(&mut client_read).await;
                    if value["method"] == "terminal/create" {
                        break value;
                    }
                };
                write_raw_response(
                    &mut client_write,
                    create_request["id"].as_i64().expect("terminal create id"),
                    json!({"terminalId": terminal_id}),
                )
                .await;
            }
            tokio::task::spawn_blocking(move || created_rx.recv_timeout(TEST_TIMEOUT))
                .await
                .expect("created barrier task")
                .expect("executor observed both durable live leases");

            let shutdown = tokio::task::spawn_blocking(move || host.shutdown());
            let terminalized = tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    let terminal_ids = persisted_surface_events(&transcript_path)
                        .into_iter()
                        .filter_map(|event| match event {
                            crate::surface::SurfaceEvent::Tool(
                                crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                                    lease:
                                        crate::surface::SurfaceRemoteTerminalLease {
                                            state:
                                                crate::surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                                    terminal_id: Some(terminal_id),
                                                    ..
                                                },
                                            ..
                                        },
                                },
                            ) => Some(terminal_id.as_str().to_string()),
                            _ => None,
                        })
                        .collect::<BTreeSet<_>>();
                    if terminal_ids.contains("terminal-live-a")
                        && terminal_ids.contains("terminal-live-b")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            continue_tx.send(()).unwrap();
            tokio::time::timeout(TEST_TIMEOUT, shutdown)
                .await
                .expect("host shutdown timeout")
                .expect("host shutdown task")
                .expect("host shutdown");

            assert!(
                terminalized,
                "host shutdown did not durably terminalize both live leases"
            );
            let events = persisted_surface_events(&transcript_path);
            let completed_count = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        crate::surface::SurfaceEvent::Tool(
                            crate::surface::ToolPatch::Completed {
                                result:
                                    crate::surface::SurfaceToolResult {
                                        tool_call_id,
                                        ..
                                    },
                            },
                        ) if tool_call_id.as_str() == "terminal-create-shared-tool"
                    )
                })
                .count();
            let result_message_count = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        crate::surface::SurfaceEvent::Item(
                            crate::surface::ItemPatch::Added {
                                item:
                                    crate::surface::SurfaceItem::ToolResultMessage {
                                        tool_call_id,
                                        ..
                                    },
                            },
                        ) if tool_call_id.as_str() == "terminal-create-shared-tool"
                    )
                })
                .count();
            assert_eq!(completed_count, 1);
            assert_eq!(result_message_count, 1);
            assert!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap().is_err(),
                "host shutdown must not resume the tool as successful"
            );

            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
        });
    }

    fn terminal_cleanup_connection_loss_is_durable(kill_succeeds: bool) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;

            let create_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            let create_id = create_request["id"].as_i64().expect("terminal create id");
            write_raw_response(
                &mut client_write,
                create_id,
                json!({"terminalId":"terminal-loss"}),
            )
            .await;
            let kill_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/kill" {
                    break value;
                }
            };
            let expected_effect = if kill_succeeds {
                let kill_id = kill_request["id"].as_i64().expect("terminal kill id");
                write_raw_response(&mut client_write, kill_id, json!({})).await;
                let release_request = loop {
                    let value = read_value(&mut client_read).await;
                    if value["method"] == "terminal/release" {
                        break value;
                    }
                };
                assert_eq!(release_request["params"]["terminalId"], "terminal-loss");
                crate::surface::ExternalEffectKind::TerminalRelease
            } else {
                crate::surface::ExternalEffectKind::TerminalKill
            };
            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            assert!(outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap().is_err());
            let _ = host.shutdown();
            assert_persisted_terminal_cleanup_ambiguous(
                &transcript_path,
                expected_effect,
                "terminal-loss",
            );
        });
    }

    #[test]
    fn decoded_write_response_survives_local_flush_ack_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let failure = Arc::new(FlushFailureSignal::default());
            let response_observed = Arc::new(Notify::new());
            let connection = tokio::task::spawn_local(run_connection_inner(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                FailWriteTextFileFlush::new(server_write, Arc::clone(&failure)),
                Some(Arc::clone(&response_observed)),
                None,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            let write_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break value;
                }
            };
            let write_id = write_request["id"].as_i64().expect("reverse request id");
            write_raw_response(&mut client_write, write_id, json!({})).await;
            tokio::time::timeout(TEST_TIMEOUT, response_observed.notified())
                .await
                .expect("write response decoded before injected flush failure");
            failure.fail();

            assert_eq!(
                tokio::task::spawn_blocking(move || outcome_rx.recv_timeout(TEST_TIMEOUT))
                    .await
                    .expect("outcome waiter")
                    .expect("write outcome"),
                Ok(()),
                "a decoded response must remain settleable when local flush acknowledgement fails"
            );
            let _ = client_write.shutdown().await;
            drop(client_read);
            let connection_error = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect_err("injected flush failure must fail the connection");
            assert!(
                matches!(connection_error, RpcFacadeError::Flush { .. }),
                "unexpected connection error: {connection_error:?}"
            );
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn decoded_terminal_kill_response_survives_local_flush_ack_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                    outcome_tx,
                    created_tx: None,
                    continue_rx: None,
                }))
                .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let failure = Arc::new(FlushFailureSignal::default());
            let response_observed = Arc::new(Notify::new());
            let connection = tokio::task::spawn_local(run_connection_inner(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                FailWriteTextFileFlush::for_terminal_cleanup(
                    server_write,
                    Arc::clone(&failure),
                ),
                Some(Arc::clone(&response_observed)),
                None,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;
            let create_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            write_raw_response(
                &mut client_write,
                create_request["id"].as_i64().expect("terminal create id"),
                json!({"terminalId":"terminal-flush-race"}),
            )
            .await;
            let kill_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/kill" {
                    break value;
                }
            };
            write_raw_response(
                &mut client_write,
                kill_request["id"].as_i64().expect("terminal kill id"),
                json!({}),
            )
            .await;
            tokio::time::timeout(TEST_TIMEOUT, response_observed.notified())
                .await
                .expect("terminal kill response decoded before injected flush failure");
            failure.fail();

            let _ = client_write.shutdown().await;
            drop(client_read);
            let connection_error = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect_err("injected flush failure must fail the connection");
            assert!(matches!(connection_error, RpcFacadeError::Flush { .. }));
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).expect("terminal outcome"),
                Err(io::ErrorKind::Other),
                "kill completion must win the flush race; only release may become ambiguous"
            );
            host.shutdown().unwrap();

            let events = persisted_surface_events(&transcript_path);
            assert!(events.iter().any(|event| matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            kind:
                                crate::surface::SurfaceCapabilityCallKind::TerminalKill,
                            state:
                                crate::surface::SurfaceCapabilityCallState::Completed {
                                    result:
                                        crate::surface::CapabilityCallResult::TerminalKillAcknowledged,
                                    ..
                                },
                            ..
                        },
                    },
                )
            )));
            assert!(!events.iter().any(|event| matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            state:
                                crate::surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind:
                                        crate::surface::ExternalEffectKind::TerminalKill,
                                    ..
                                },
                            ..
                        },
                    },
                )
            )));
            assert_persisted_terminal_cleanup_ambiguous(
                &transcript_path,
                crate::surface::ExternalEffectKind::TerminalRelease,
                "terminal-flush-race",
            );
        });
    }

    #[test]
    fn connection_loss_after_write_delivery_reports_ambiguous_effect_without_success() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break;
                }
            }

            client_write.shutdown().await.unwrap();
            drop(client_write);
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::Other),
                "an acknowledged physical write without a response must not report success"
            );
            host.shutdown().unwrap();
            assert_persisted_external_effect_ambiguity(&transcript_path);
        });
    }

    #[test]
    fn terminal_create_connection_loss_persists_unknown_identity_and_never_retries() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break;
                }
            }

            client_write.shutdown().await.unwrap();
            drop(client_write);
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::Other),
                "a possibly-created terminal without an id must not report success"
            );
            host.shutdown().unwrap();
            assert_persisted_terminal_create_identity_unknown(&transcript_path);
        });
    }

    #[test]
    fn cancelling_delivered_write_preserves_external_effect_ambiguity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break;
                }
            }

            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;
            let prompt = read_response(&mut client_read, 3).await;
            assert!(
                prompt["error"]["data"]
                    .as_str()
                    .is_some_and(|message| message.contains("ambiguous")),
                "delivered write cancellation must surface ambiguity: {prompt}"
            );
            assert!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap().is_err(),
                "cancelled delivered write must never report success"
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
            assert_persisted_external_effect_ambiguity(&transcript_path);
        });
    }

    #[test]
    fn cancelling_delivered_terminal_create_preserves_unknown_identity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host = RuntimeHost::start_with_executor(Arc::new(TerminalCreateExecutor {
                outcome_tx,
                created_tx: None,
                continue_rx: None,
            }))
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("create terminal".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break;
                }
            }
            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;
            let prompt = read_response(&mut client_read, 3).await;
            assert!(
                prompt["error"]["data"]
                    .as_str()
                    .is_some_and(|message| message.contains("ambiguous")),
                "delivered terminal create cancellation must surface ambiguity: {prompt}"
            );
            assert!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap().is_err(),
                "cancelled terminal create must never report success"
            );
            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
            assert_persisted_terminal_create_identity_unknown(&transcript_path);
        });
    }

    #[test]
    fn host_shutdown_preserves_delivered_write_external_effect_ambiguity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let written = Arc::new(Notify::new());
            let connection = tokio::task::spawn_local(run_connection_inner(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
                None,
                Some(Arc::clone(&written)),
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break;
                }
            }
            tokio::time::timeout(TEST_TIMEOUT, written.notified())
                .await
                .expect("write delivery acknowledgement");

            tokio::task::spawn_blocking(move || host.shutdown())
                .await
                .expect("host shutdown task")
                .expect("host shutdown");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::Interrupted)
            );
            assert_persisted_external_effect_ambiguity(&transcript_path);

            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
        });
    }

    #[test]
    fn failed_delivery_checkpoint_prevents_write_request_from_reaching_wire() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            crate::runtime_surface::JsonlSurfaceCommitLedger::
                inject_capability_delivery_checkpoint_failures(transcript_path, 3);

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            let mut outcome_task =
                tokio::task::spawn_blocking(move || outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap());
            let outcome = loop {
                tokio::select! {
                    outcome = &mut outcome_task => break outcome.expect("outcome task"),
                    value = read_value(&mut client_read) => {
                        assert_ne!(
                            value["method"], "fs/write_text_file",
                            "wire write occurred before the durable delivery barrier"
                        );
                    }
                }
            };
            assert_eq!(outcome, Err(io::ErrorKind::Other));

            client_write.shutdown().await.unwrap();
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn cancelling_prompt_terminalizes_outstanding_read_text_file_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (content_tx, _content_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(ReadTextFileExecutor { content_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().read_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("read notes".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/read_text_file" {
                    break;
                }
            }

            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;
            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(
                prompt["result"]["stopReason"], "cancelled",
                "unexpected prompt response: {prompt}"
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn cancelling_prompt_terminalizes_outstanding_terminal_output_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host =
                RuntimeHost::start_with_executor(Arc::new(TerminalOutputCancelExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new().terminal(true)),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;
            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("observe until cancelled".to_string())],
                ),
            )
            .await;
            let create = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/create" {
                    break value;
                }
            };
            write_raw_response(
                &mut client_write,
                create["id"].as_i64().expect("terminal create id"),
                json!({"terminalId":"terminal-cancel-output"}),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "terminal/output" {
                    break;
                }
            }
            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;
            let prompt = read_response(&mut client_read, 3).await;
            assert!(
                prompt["error"]["data"]
                    .as_str()
                    .is_some_and(|message| message.contains("ambiguous")),
                "terminal cleanup ambiguity must remain visible after cancel: {prompt}"
            );
            assert_persisted_terminal_observation(
                &transcript_path,
                crate::surface::SurfaceCapabilityCallKind::TerminalOutput,
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    async fn write_request(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        id: i64,
        method: &str,
        params: impl Serialize,
    ) {
        let mut encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
    }

    async fn write_notification(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        method: &str,
        params: impl Serialize,
    ) {
        let mut encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
    }

    async fn write_raw_response(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        id: i64,
        result: Value,
    ) {
        let mut encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
    }

    async fn read_response<R>(reader: &mut BufReader<R>, id: i64) -> Value
    where
        R: AsyncRead + Unpin,
    {
        loop {
            let value = read_value(reader).await;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    }

    async fn read_value<R>(reader: &mut BufReader<R>) -> Value
    where
        R: AsyncRead + Unpin,
    {
        let mut line = String::new();
        tokio::time::timeout(TEST_TIMEOUT, reader.read_line(&mut line))
            .await
            .expect("ACP frame timeout")
            .expect("ACP frame read");
        assert!(!line.is_empty(), "ACP connection closed before next frame");
        serde_json::from_str(&line).unwrap()
    }

    fn persisted_surface_events(path: &std::path::Path) -> Vec<crate::surface::SurfaceEvent> {
        let records = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        let committed_ids = records
            .iter()
            .filter(|record| record["type"] == "runtime.surface_commit.committed")
            .filter_map(|record| record.get("commit_id").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();

        records
            .into_iter()
            .filter(|record| record["type"] == "runtime.surface_commit.prepared")
            .filter(|record| {
                record
                    .get("commit_id")
                    .and_then(Value::as_str)
                    .is_some_and(|commit_id| committed_ids.contains(commit_id))
            })
            .filter_map(|record| record.get("batch").cloned())
            .filter_map(|batch| {
                serde_json::from_value::<crate::runtime_surface::StoredSurfaceCommitBatchV1>(batch)
                    .ok()
            })
            .filter_map(|batch| batch.into_live().ok())
            .flat_map(|batch| {
                batch
                    .events
                    .as_slice()
                    .iter()
                    .map(|event| event.event.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn assert_persisted_external_effect_ambiguity(path: &std::path::Path) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(crate::surface::ToolPatch::Completed {
                    result: crate::surface::SurfaceToolResult {
                        terminal: crate::surface::SurfaceToolTerminal {
                            kind: crate::surface::SurfaceToolResultKind::ExternalEffectAmbiguous,
                            ..
                        },
                        ..
                    },
                })
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Operation(
                    crate::surface::OperationPatch::GenerationStopped {
                        reason:
                            crate::surface::GenerationStopReason::ExecutionFailed {
                                class: crate::surface::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                                ..
                            },
                        ..
                    }
                )
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Operation(crate::surface::OperationPatch::Terminal {
                    record: crate::surface::OperationTerminalRecord {
                        terminal: crate::surface::OperationTerminal::Failed {
                            class: crate::surface::FailureClass::ExternalEffectAmbiguous,
                            ..
                        },
                        ..
                    },
                })
            )
        }));
    }

    fn assert_persisted_terminal_create_identity_unknown(path: &std::path::Path) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            kind:
                                crate::surface::SurfaceCapabilityCallKind::TerminalCreate,
                            state:
                                crate::surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind:
                                        crate::surface::ExternalEffectKind::TerminalCreate,
                                    ..
                                },
                            ..
                        },
                    },
                )
            )
        }));
        assert!(
            events.iter().any(|event| {
                matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: crate::surface::SurfaceRemoteTerminalLease {
                            state:
                                crate::surface::SurfaceRemoteTerminalLeaseState::IdentityUnknown {
                                    ..
                                },
                            ..
                        },
                    },
                )
            )
            })
        );
        assert_persisted_external_effect_ambiguity(path);
    }

    fn assert_persisted_terminal_observation(
        path: &std::path::Path,
        expected_kind: crate::surface::SurfaceCapabilityCallKind,
    ) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            kind,
                            state:
                                crate::surface::SurfaceCapabilityCallState::Completed {
                                    ..
                                }
                                | crate::surface::SurfaceCapabilityCallState::ObservationUnavailable {
                                    ..
                                },
                            ..
                        },
                    },
                ) if kind == &expected_kind
            )
        }));
    }

    fn assert_persisted_terminal_wait_limit_state(path: &std::path::Path, expect_completed: bool) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            kind: crate::surface::SurfaceCapabilityCallKind::TerminalWaitForExit,
                            state: crate::surface::SurfaceCapabilityCallState::Completed { .. },
                            ..
                        },
                    },
                )
            ) == expect_completed
                && matches!(
                    event,
                    crate::surface::SurfaceEvent::Tool(
                        crate::surface::ToolPatch::CapabilityCallChanged {
                            call: crate::surface::SurfaceCapabilityCall {
                                kind: crate::surface::SurfaceCapabilityCallKind::TerminalWaitForExit,
                                state:
                                    crate::surface::SurfaceCapabilityCallState::Completed { .. }
                                    | crate::surface::SurfaceCapabilityCallState::ObservationUnavailable { .. },
                                ..
                            },
                        },
                    )
                )
        }));
    }

    fn assert_persisted_capability_written(
        path: &std::path::Path,
        expected_kind: crate::surface::SurfaceCapabilityCallKind,
    ) {
        assert!(persisted_capability_is_written(path, expected_kind));
    }

    fn persisted_capability_is_written(
        path: &std::path::Path,
        expected_kind: crate::surface::SurfaceCapabilityCallKind,
    ) -> bool {
        let events = persisted_surface_events(path);
        events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            kind,
                            state:
                                crate::surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                            ..
                        },
                    },
                ) if kind == &expected_kind
            )
        })
    }

    fn assert_persisted_terminal_released(path: &std::path::Path, expected_terminal_id: &str) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            kind:
                                crate::surface::SurfaceCapabilityCallKind::TerminalCreate,
                            state:
                                crate::surface::SurfaceCapabilityCallState::Completed {
                                    result:
                                        crate::surface::CapabilityCallResult::TerminalCreated {
                                            terminal_id,
                                        },
                                    ..
                                },
                            ..
                        },
                    },
                ) if terminal_id.as_str() == expected_terminal_id
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: crate::surface::SurfaceRemoteTerminalLease {
                            state:
                                crate::surface::SurfaceRemoteTerminalLeaseState::Live {
                                    terminal_id,
                                    ..
                                },
                            ..
                        },
                    },
                ) if terminal_id.as_str() == expected_terminal_id
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: crate::surface::SurfaceRemoteTerminalLease {
                            state: crate::surface::SurfaceRemoteTerminalLeaseState::Released,
                            ..
                        },
                    },
                )
            )
        }));
    }

    fn assert_persisted_terminal_cleanup_ambiguous(
        path: &std::path::Path,
        expected_effect: crate::surface::ExternalEffectKind,
        expected_terminal_id: &str,
    ) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::CapabilityCallChanged {
                        call: crate::surface::SurfaceCapabilityCall {
                            state:
                                crate::surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind,
                                    ..
                                },
                            ..
                        },
                    },
                ) if *effect_kind == expected_effect
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(
                    crate::surface::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: crate::surface::SurfaceRemoteTerminalLease {
                            state:
                                crate::surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                    terminal_id: Some(terminal_id),
                                    ..
                                },
                            ..
                        },
                    },
                ) if terminal_id.as_str() == expected_terminal_id
            )
        }));
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
            max_budget_usd: None,
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
