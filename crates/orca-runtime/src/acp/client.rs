//! Standard ACP SDK connection used by the hosted TUI and headless attach.
//!
//! The SDK runs its handlers on `Send` tasks; attach clients keep their state
//! on a `LocalSet`. The handlers only queue what arrives, and one local pump
//! hands it to the [`ClientHandler`] in arrival order.

use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
};
use agent_client_protocol::{Agent, ByteStreams, ConnectionTo, JsonRpcRequest, Responder};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use super::legacy_model::{SetSessionModelRequest, SetSessionModelResponse};

/// What an attach client does with the daemon's requests and notifications.
#[async_trait::async_trait(?Send)]
pub trait ClientHandler {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse>;

    async fn session_notification(
        &self,
        notification: SessionNotification,
    ) -> agent_client_protocol::Result<()>;
}

enum Incoming {
    Permission(
        RequestPermissionRequest,
        Responder<RequestPermissionResponse>,
    ),
    Notification(SessionNotification),
    /// Answered once everything queued before it has been handled.
    Barrier(oneshot::Sender<()>),
}

/// Requests and notifications an attach client sends to the daemon.
#[derive(Clone)]
pub struct AgentHandle {
    connection: ConnectionTo<Agent>,
    incoming: mpsc::UnboundedSender<Incoming>,
}

impl AgentHandle {
    pub async fn initialize(
        &self,
        request: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        self.request(request).await
    }

    pub async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        self.request(request).await
    }

    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        self.request(request).await
    }

    pub async fn prompt(
        &self,
        request: PromptRequest,
    ) -> agent_client_protocol::Result<PromptResponse> {
        self.request(request).await
    }

    pub async fn set_session_model(
        &self,
        request: SetSessionModelRequest,
    ) -> agent_client_protocol::Result<SetSessionModelResponse> {
        self.request(request).await
    }

    pub async fn set_session_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        self.request(request).await
    }

    pub async fn cancel(
        &self,
        notification: CancelNotification,
    ) -> agent_client_protocol::Result<()> {
        self.connection.send_notification(notification)
    }

    async fn request<R: JsonRpcRequest>(
        &self,
        request: R,
    ) -> agent_client_protocol::Result<R::Response> {
        let result = self.connection.send_request(request).block_task().await;
        // The daemon sends a turn's updates before the turn's response. Let the
        // client handle everything that arrived first.
        let (done, handled) = oneshot::channel();
        if self.incoming.send(Incoming::Barrier(done)).is_ok() {
            let _ = handled.await;
        }
        result
    }
}

pub struct Connection {
    pub agent: AgentHandle,
    driver: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    pump: tokio::task::JoinHandle<()>,
}

pub struct AttachedSession {
    pub session_id: SessionId,
    pub startup_warnings: Vec<String>,
}

const MAX_READINESS_WARNINGS: usize = 16;
const MAX_READINESS_WARNING_BYTES: usize = 8 * 1024;

pub fn readiness_warnings(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<String> {
    let Some(readiness) = meta.and_then(|meta| meta.get(super::READINESS_META)) else {
        return Vec::new();
    };
    if readiness.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Vec::new();
    }
    readiness
        .get("startupWarnings")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|warning| warning.len() <= MAX_READINESS_WARNING_BYTES)
        .take(MAX_READINESS_WARNINGS)
        .map(str::to_string)
        .collect()
}

impl Connection {
    #[cfg(unix)]
    pub async fn connect(
        socket: &Path,
        client: Rc<dyn ClientHandler>,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        let stream = super::daemon::connect(socket).await?;
        let (read, write) = stream.into_split();
        Self::connect_streams(read, write, client, capabilities).await
    }

    #[cfg(not(unix))]
    pub async fn connect(
        _socket: &Path,
        _client: Rc<dyn ClientHandler>,
        _capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        Err(super::daemon::unsupported())
    }

    /// Starts the connection on a `LocalSet` and initializes it.
    pub(crate) async fn connect_streams<R, W>(
        read: R,
        write: W,
        client: Rc<dyn ClientHandler>,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let (incoming, mut queued) = mpsc::unbounded_channel();
        let (opened, connection) = oneshot::channel();
        let permissions = incoming.clone();
        let notifications = incoming.clone();
        let driver = tokio::task::spawn_local(
            agent_client_protocol::Client
                .builder()
                .on_receive_request(
                    async move |request: RequestPermissionRequest,
                                responder: Responder<RequestPermissionResponse>,
                                _cx| {
                        let _ = permissions.send(Incoming::Permission(request, responder));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        let _ = notifications.send(Incoming::Notification(notification));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    ByteStreams::new(write.compat_write(), read.compat()),
                    async move |cx: ConnectionTo<Agent>| {
                        let _ = opened.send(cx.clone());
                        // The connection ends when the daemon hangs up or the
                        // `Connection` is dropped (which aborts this task).
                        cx.incoming_closed().await;
                        Ok(())
                    },
                ),
        );
        let pump = tokio::task::spawn_local(async move {
            while let Some(message) = queued.recv().await {
                match message {
                    Incoming::Notification(notification) => {
                        let _ = client.session_notification(notification).await;
                    }
                    Incoming::Permission(request, responder) => {
                        let client = Rc::clone(&client);
                        tokio::task::spawn_local(async move {
                            let result = client.request_permission(request).await;
                            let _ = responder.respond_with_result(result);
                        });
                    }
                    Incoming::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        let connection = match connection.await {
            Ok(connection) => connection,
            Err(_) => {
                pump.abort();
                return Err(io::Error::other("ACP connection closed before it opened"));
            }
        };
        let connection = Self {
            agent: AgentHandle {
                connection,
                incoming,
            },
            driver,
            pump,
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            connection.agent.initialize(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(capabilities)
                    .client_info(Implementation::new(
                        "orca-hosted-client",
                        env!("CARGO_PKG_VERSION"),
                    )),
            ),
        )
        .await?
        .map_err(io::Error::other)?;
        Ok(connection)
    }

    pub fn is_closed(&self) -> bool {
        self.driver.is_finished()
    }

    /// `new` creates a session; all other selectors must be exact session IDs.
    /// Never retry a prompt automatically: a lost response is not a failed turn.
    pub async fn attach(&self, cwd: &Path, selector: &str) -> io::Result<SessionId> {
        self.attach_with_metadata(cwd, selector)
            .await
            .map(|attached| attached.session_id)
    }

    pub async fn attach_with_metadata(
        &self,
        cwd: &Path,
        selector: &str,
    ) -> io::Result<AttachedSession> {
        tokio::time::timeout(Duration::from_secs(30), async {
            if selector == "new" {
                self.agent
                    .new_session(NewSessionRequest::new(cwd.to_path_buf()))
                    .await
                    .map(|response| AttachedSession {
                        startup_warnings: readiness_warnings(response.meta.as_ref()),
                        session_id: response.session_id,
                    })
            } else {
                let id = SessionId::new(selector);
                self.agent
                    .load_session(LoadSessionRequest::new(id.clone(), cwd.to_path_buf()))
                    .await
                    .map(|response| AttachedSession {
                        startup_warnings: readiness_warnings(response.meta.as_ref()),
                        session_id: id,
                    })
            }
        })
        .await?
        .map_err(io::Error::other)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
        self.pump.abort();
    }
}

struct HeadlessClient;

#[async_trait::async_trait(?Send)]
impl ClientHandler for HeadlessClient {
    async fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        // Non-interactive attachment never silently grants permission.
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(
        &self,
        note: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        use std::io::Write;
        if let SessionUpdate::AgentMessageChunk(chunk) = note.update
            && let ContentBlock::Text(text) = chunk.content
        {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(text.text.as_bytes())
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            stdout
                .flush()
                .map_err(agent_client_protocol::Error::into_internal_error)?;
        }
        Ok(())
    }
}

pub fn run_headless(socket: &Path, cwd: &Path, selector: &str, prompt: String) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&runtime, async {
        let connection =
            Connection::connect(socket, Rc::new(HeadlessClient), ClientCapabilities::new()).await?;
        let attached = connection.attach_with_metadata(cwd, selector).await?;
        for warning in &attached.startup_warnings {
            eprintln!("orca: warning: {warning}");
        }
        let id = attached.session_id;
        eprintln!("orca: attached ACP session {id}");
        let response = connection
            .agent
            .prompt(PromptRequest::new(id, vec![prompt.into()]))
            .await
            .map_err(io::Error::other)?;
        println!();
        if response.stop_reason == StopReason::EndTurn {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "ACP turn stopped: {:?}",
                response.stop_reason
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

    use super::*;

    #[test]
    fn readiness_metadata_extracts_only_string_warnings() {
        let meta = serde_json::Map::from_iter([(
            super::super::READINESS_META.to_string(),
            serde_json::json!({
                "version": 1,
                "startupWarnings": ["shell unavailable", 7, null],
            }),
        )]);

        assert_eq!(
            readiness_warnings(Some(&meta)),
            vec!["shell unavailable".to_string()]
        );
        assert!(readiness_warnings(None).is_empty());

        let incompatible = serde_json::Map::from_iter([(
            super::super::READINESS_META.to_string(),
            serde_json::json!({
                "version": 2,
                "startupWarnings": ["must be ignored"],
            }),
        )]);
        assert!(readiness_warnings(Some(&incompatible)).is_empty());
    }

    /// Records the text of every agent message chunk, taking `delay` over each.
    #[derive(Default)]
    struct Recorder {
        texts: RefCell<Vec<String>>,
        delay: Duration,
    }

    #[async_trait::async_trait(?Send)]
    impl ClientHandler for Recorder {
        async fn request_permission(
            &self,
            _request: RequestPermissionRequest,
        ) -> agent_client_protocol::Result<RequestPermissionResponse> {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        }

        async fn session_notification(
            &self,
            notification: SessionNotification,
        ) -> agent_client_protocol::Result<()> {
            tokio::time::sleep(self.delay).await;
            if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                && let ContentBlock::Text(text) = chunk.content
            {
                self.texts.borrow_mut().push(text.text);
            }
            Ok(())
        }
    }

    /// The daemon end of a connection, scripted by the test.
    struct Daemon {
        read: BufReader<ReadHalf<DuplexStream>>,
        write: WriteHalf<DuplexStream>,
    }

    impl Daemon {
        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut line))
                .await
                .expect("client frame timeout")
                .unwrap();
            assert_ne!(read, 0, "client closed the connection");
            serde_json::from_str(&line).unwrap()
        }

        async fn send(&mut self, frame: Value) {
            let mut bytes = serde_json::to_vec(&frame).unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }
    }

    /// Connects `client` to a scripted daemon that answers `initialize`.
    async fn connected(client: Rc<dyn ClientHandler>) -> (Connection, Daemon) {
        let (client_end, daemon_end) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_end);
        let mut daemon = Daemon {
            read: BufReader::new(daemon_read),
            write: daemon_write,
        };
        let connecting = tokio::task::spawn_local(Connection::connect_streams(
            client_read,
            client_write,
            client,
            ClientCapabilities::new(),
        ));
        let initialize = daemon.recv().await;
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["clientInfo"]["name"],
            "orca-hosted-client"
        );
        daemon
            .send(
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
        (connecting.await.unwrap().unwrap(), daemon)
    }

    fn run_local(test: impl std::future::Future<Output = ()>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, test);
    }

    #[test]
    fn set_model_and_cancel_keep_their_wire_shape() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let set_model = tokio::task::spawn_local(async move {
                agent
                    .set_session_model(SetSessionModelRequest::new("s-1", "deepseek-v4-pro"))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/set_model");
            assert_eq!(
                request["params"],
                json!({"sessionId": "s-1", "modelId": "deepseek-v4-pro"})
            );
            daemon
                .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {}}))
                .await;
            set_model.await.unwrap().unwrap();

            connection
                .agent
                .cancel(CancelNotification::new("s-1"))
                .await
                .unwrap();
            assert_eq!(
                daemon.recv().await,
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s-1"}})
            );
        });
    }

    #[test]
    fn a_daemon_hangup_fails_the_pending_prompt_and_closes_the_connection() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            assert_eq!(daemon.recv().await["method"], "session/prompt");
            drop(daemon);
            let result = tokio::time::timeout(Duration::from_secs(5), prompt)
                .await
                .expect("the prompt settles")
                .unwrap();
            assert!(
                result.is_err(),
                "a lost turn is not a finished one: {result:?}"
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !connection.is_closed() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the connection closes");
        });
    }

    #[test]
    fn updates_sent_before_a_response_are_handled_before_it_returns() {
        run_local(async {
            let recorder = Rc::new(Recorder {
                delay: Duration::from_millis(20),
                ..Recorder::default()
            });
            let (connection, mut daemon) = connected(recorder.clone()).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/prompt");
            for text in ["one", "two"] {
                daemon
                    .send(
                        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                            "sessionId": "s-1",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": text},
                            },
                        }}),
                    )
                    .await;
            }
            daemon
                .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "end_turn"}}))
                .await;
            let response = prompt.await.unwrap().unwrap();
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            assert_eq!(*recorder.texts.borrow(), ["one", "two"]);
        });
    }

    #[test]
    fn a_request_dropped_while_outstanding_is_cancelled_on_the_wire() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/prompt");
            prompt.abort();
            // The daemon answers no notification, so it ignores this one.
            assert_eq!(
                daemon.recv().await,
                json!({
                    "jsonrpc": "2.0",
                    "method": "$/cancel_request",
                    "params": {"requestId": request["id"]},
                })
            );
        });
    }
}
