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

#[cfg(test)]
mod tests {
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
}
