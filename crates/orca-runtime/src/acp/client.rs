//! Standard ACP SDK connection used by the hosted TUI and headless attach.

use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol::{
    Agent, Client, ClientCapabilities, ClientSideConnection, Implementation, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SessionUpdate,
};

pub struct Connection {
    pub agent: Rc<ClientSideConnection>,
    io: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
}

impl Connection {
    #[cfg(unix)]
    pub async fn connect(
        socket: &Path,
        client: impl Client + 'static,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let stream = super::daemon::connect(socket).await?;
        let (read, write) = stream.into_split();
        let (agent, io_task) =
            ClientSideConnection::new(client, write.compat_write(), read.compat(), |future| {
                tokio::task::spawn_local(future);
            });
        let connection = Self {
            agent: Rc::new(agent),
            io: tokio::task::spawn_local(io_task),
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

    #[cfg(not(unix))]
    pub async fn connect(
        _socket: &Path,
        _client: impl Client + 'static,
        _capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        Err(super::daemon::unsupported())
    }

    pub fn is_closed(&self) -> bool {
        self.io.is_finished()
    }

    /// `new` creates a session; all other selectors must be exact session IDs.
    /// Never retry a prompt automatically: a lost response is not a failed turn.
    pub async fn attach(&self, cwd: &Path, selector: &str) -> io::Result<SessionId> {
        tokio::time::timeout(Duration::from_secs(30), async {
            if selector == "new" {
                self.agent
                    .new_session(NewSessionRequest::new(cwd.to_path_buf()))
                    .await
                    .map(|response| response.session_id)
            } else {
                let id = SessionId::new(selector);
                self.agent
                    .load_session(LoadSessionRequest::new(id.clone(), cwd.to_path_buf()))
                    .await
                    .map(|_| id)
            }
        })
        .await?
        .map_err(io::Error::other)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.io.abort();
    }
}

struct HeadlessClient;

#[async_trait::async_trait(?Send)]
impl Client for HeadlessClient {
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
            && let agent_client_protocol::ContentBlock::Text(text) = chunk.content
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
            Connection::connect(socket, HeadlessClient, ClientCapabilities::new()).await?;
        let id = connection.attach(cwd, selector).await?;
        eprintln!("orca: attached ACP session {id}");
        let response = connection
            .agent
            .prompt(PromptRequest::new(id, vec![prompt.into()]))
            .await
            .map_err(io::Error::other)?;
        println!();
        if response.stop_reason == agent_client_protocol::StopReason::EndTurn {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "ACP turn stopped: {:?}",
                response.stop_reason
            )))
        }
    })
}
