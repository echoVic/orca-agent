//! The HTTP+SSE transport of MCP protocol 2024-11-05, which servers written
//! before streamable HTTP still speak. A GET opens an event stream whose
//! first `endpoint` event names the url to POST messages to. The server only
//! acknowledges each POST: the response arrives later, as a `message` event
//! on the stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use url::Url;

use orca_core::mcp_types::McpServerConfig;

use crate::auth::{AuthAttempt, RemoteAuth};
use crate::sse::{SseDecoder, SseEvent};
use crate::transport::{
    MAX_SSE_RESPONSE_BYTES, McpElicitationHandler, McpTransport, configured_headers,
    elicitation_not_supported, format_duration, initialize_params, is_elicitation_create_request,
    is_server_request, json_rpc_id_to_string, list_params, negotiated_protocol_version,
    parse_terminal_message, remote_url, resolve_sse_elicitation, server_request_reply,
    timeout_from_ms,
};

/// What every request fails with once the event stream has ended. The client
/// reconnects when it sees it.
pub(crate) const MCP_SSE_EVENT_STREAM_CLOSED: &str = "MCP SSE event stream closed";

const MCP_TOOL_CALL_CANCELLED: &str = "MCP tool call cancelled";
const EVENT_STREAM: &str = "text/event-stream";
/// How often a waiting request checks whether it has been cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// Talks to a server over legacy SSE. A background thread reads the event
/// stream and hands each response to the request waiting for it; each
/// request waits with its own timeout.
pub(crate) struct LegacySseTransport {
    server_name: String,
    /// Where messages are POSTed: the endpoint the event stream named.
    endpoint: Url,
    /// The configured headers, sent with every request along with the auth.
    headers: HeaderMap,
    auth: Arc<RemoteAuth>,
    client: reqwest::blocking::Client,
    stream: Arc<EventStream>,
    next_id: AtomicU64,
    startup_timeout: Duration,
    tool_timeout: Duration,
    /// Stops the reader when the transport is dropped.
    _reader: ReaderThread,
}

impl LegacySseTransport {
    /// Opens the server's event stream, then waits up to the startup timeout
    /// for the endpoint it names. When the server refuses the stream with
    /// 401, the token is refreshed, once, and the stream opened again.
    pub(crate) fn connect(config: &McpServerConfig, auth: Arc<RemoteAuth>) -> Result<Self, String> {
        let base = Url::parse(&remote_url(config)?)
            .map_err(|error| format!("MCP server '{}' has an invalid url: {error}", config.name))?;
        let headers = configured_headers(config)?;
        let startup_timeout = timeout_from_ms(config.startup_timeout_ms);
        let mut attempt = auth.begin()?;
        let (stream, reader, endpoint) = loop {
            let mut stream_headers = headers.clone();
            attempt.apply(&mut stream_headers);
            let stream = Arc::new(EventStream::default());
            let (announce, announced) = mpsc::channel();
            let reader = ReaderThread::spawn(EventStreamReader {
                server_name: config.name.clone(),
                base: base.clone(),
                headers: stream_headers,
                stream: Arc::clone(&stream),
                announce: Some(announce),
                endpoint: None,
                unauthorized: false,
                reply_timeout: startup_timeout,
            })?;
            match announced.recv_timeout(startup_timeout) {
                Ok(Ok(endpoint)) => break (stream, reader, endpoint),
                Ok(Err(refusal)) if refusal.unauthorized => {
                    match auth.after_unauthorized(&attempt)? {
                        Some(retry) => attempt = retry,
                        None => return Err(refusal.reason),
                    }
                }
                Ok(Err(refusal)) => return Err(refusal.reason),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "MCP server '{}' sent no SSE endpoint within {}",
                        config.name,
                        format_duration(startup_timeout)
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "MCP server '{}' closed its SSE event stream before sending an endpoint",
                        config.name
                    ));
                }
            }
        };
        Ok(Self {
            server_name: config.name.clone(),
            endpoint,
            headers,
            auth,
            client: crate::http::blocking_client(),
            stream,
            next_id: AtomicU64::new(1),
            startup_timeout,
            tool_timeout: timeout_from_ms(config.tool_timeout_ms),
            _reader: reader,
        })
    }

    /// Sends one request and waits for its response on the event stream,
    /// answering the server's questions in the meantime.
    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err(MCP_TOOL_CALL_CANCELLED.to_string());
        }
        let deadline = Instant::now() + timeout;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Wait before sending: the response may arrive before the POST is
        // acknowledged.
        let waiter = self.stream.wait_for(id)?;
        let what = format!("request '{method}'");
        self.post(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            deadline,
        )
        .map_err(|failure| failure.describe(&what, timeout))?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Failure::TimedOut.describe(&what, timeout));
            }
            match waiter.receiver.recv_timeout(remaining.min(CANCEL_POLL)) {
                Ok(StreamMessage::Response(response)) => {
                    return parse_terminal_message(response, method, id);
                }
                Ok(StreamMessage::Request(request)) => {
                    let reply = match handler {
                        Some(handler) => {
                            resolve_sse_elicitation(&self.server_name, &request, Some(handler))
                        }
                        None => elicitation_not_supported(&request),
                    };
                    self.post(&reply, deadline)
                        .map_err(|failure| failure.describe("elicitation reply", timeout))?;
                }
                Ok(StreamMessage::Closed(reason)) => return Err(reason),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if should_cancel() {
                        return Err(MCP_TOOL_CALL_CANCELLED.to_string());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(MCP_SSE_EVENT_STREAM_CLOSED.to_string());
                }
            }
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        if let Some(reason) = self.stream.closed_reason() {
            return Err(reason);
        }
        let deadline = Instant::now() + self.startup_timeout;
        self.post(
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
            deadline,
        )
        .map_err(|failure| failure.describe(&format!("notify '{method}'"), self.startup_timeout))
    }

    /// POSTs one message to the endpoint. Any 2xx will do: an answer, if one
    /// is due, comes on the event stream. When the server answers 401, the
    /// token is refreshed, once, and the message POSTed again.
    fn post(&self, message: &Value, deadline: Instant) -> Result<(), Failure> {
        let mut attempt = self.auth.begin().map_err(Failure::Auth)?;
        loop {
            match self.post_with(&attempt, message, deadline) {
                Err(Failure::Status(status)) if status == StatusCode::UNAUTHORIZED => {
                    match self
                        .auth
                        .after_unauthorized(&attempt)
                        .map_err(Failure::Auth)?
                    {
                        Some(retry) => attempt = retry,
                        None => return Err(Failure::Status(status)),
                    }
                }
                result => return result,
            }
        }
    }

    fn post_with(
        &self,
        auth: &AuthAttempt,
        message: &Value,
        deadline: Instant,
    ) -> Result<(), Failure> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(Failure::TimedOut);
        }
        let mut headers = self.headers.clone();
        auth.apply(&mut headers);
        let response = self
            .client
            .post(self.endpoint.clone())
            .headers(headers)
            .timeout(timeout)
            .json(message)
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    Failure::TimedOut
                } else {
                    Failure::Failed(error.to_string())
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure::Status(status));
        }
        Ok(())
    }
}

impl McpTransport for LegacySseTransport {
    fn initialize(&self) -> Result<Value, String> {
        let result = self.request(
            "initialize",
            initialize_params(),
            self.startup_timeout,
            None,
            &never_cancelled,
        )?;
        negotiated_protocol_version(&self.server_name, &result)?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    fn list_tools(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request(
            "tools/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            &never_cancelled,
        )
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.call_tool_with_elicitation_handler(name, arguments, None)
    }

    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.call_tool_with_elicitation_handler_or_cancel(
            name,
            arguments,
            handler,
            &never_cancelled,
        )
    }

    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            self.tool_timeout,
            handler,
            should_cancel,
        )
    }

    fn list_resources(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.list_resources_or_cancel(cursor, &never_cancelled)
    }

    fn list_resources_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request(
            "resources/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            should_cancel,
        )
    }

    fn list_resource_templates(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.list_resource_templates_or_cancel(cursor, &never_cancelled)
    }

    fn list_resource_templates_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request(
            "resources/templates/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            should_cancel,
        )
    }

    fn read_resource(&self, uri: &str) -> Result<Value, String> {
        self.read_resource_or_cancel(uri, &never_cancelled)
    }

    fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request(
            "resources/read",
            json!({"uri": uri}),
            self.tool_timeout,
            None,
            should_cancel,
        )
    }

    fn list_prompts(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request(
            "prompts/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            &never_cancelled,
        )
    }

    fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request(
            "prompts/get",
            json!({"name": name, "arguments": arguments}),
            self.tool_timeout,
            None,
            &never_cancelled,
        )
    }
}

/// What requests that cannot be cancelled pass for `should_cancel`.
fn never_cancelled() -> bool {
    false
}

/// Why sending a message, or waiting for its answer, failed.
enum Failure {
    TimedOut,
    Status(StatusCode),
    Failed(String),
    /// The server needs a login Orca cannot make. The message says so, and
    /// is reported as it is.
    Auth(String),
}

impl Failure {
    /// Words the failure for the user. `what` names what was sent, as in
    /// "request 'tools/list'".
    fn describe(self, what: &str, timeout: Duration) -> String {
        match self {
            Self::TimedOut => format!(
                "MCP SSE {what} timed out after {}",
                format_duration(timeout)
            ),
            Self::Status(status) => format!("MCP SSE {what} failed with {status}"),
            Self::Failed(error) => format!("MCP SSE {what} failed: {error}"),
            Self::Auth(error) => error,
        }
    }
}

/// What the reader shares with the requests waiting on it.
#[derive(Default)]
struct EventStream {
    waiters: Mutex<Waiters>,
}

#[derive(Default)]
struct Waiters {
    /// Each waiting request's channel, by its JSON-RPC id.
    pending: HashMap<String, mpsc::Sender<StreamMessage>>,
    /// Why the stream ended, once it has.
    closed: Option<String>,
}

/// What the reader hands a waiting request.
enum StreamMessage {
    /// Its response.
    Response(Value),
    /// A request from the server, for it to answer.
    Request(Value),
    /// The stream ended, for the reason given.
    Closed(String),
}

impl EventStream {
    fn waiters(&self) -> MutexGuard<'_, Waiters> {
        self.waiters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts waiting for the response to request `id`, unless the stream
    /// has already ended.
    fn wait_for(&self, id: u64) -> Result<Waiter<'_>, String> {
        let (sender, receiver) = mpsc::channel();
        let mut waiters = self.waiters();
        if let Some(reason) = &waiters.closed {
            return Err(reason.clone());
        }
        let key = id.to_string();
        waiters.pending.insert(key.clone(), sender);
        Ok(Waiter {
            stream: self,
            key,
            receiver,
        })
    }

    fn closed_reason(&self) -> Option<String> {
        self.waiters().closed.clone()
    }

    /// Hands a response to the request waiting for it, if one is.
    fn deliver(&self, id: &str, response: Value) {
        if let Some(sender) = self.waiters().pending.remove(id) {
            let _ = sender.send(StreamMessage::Response(response));
        }
    }

    /// Hands a request from the server to a waiting request to answer.
    /// Returns false when no request is waiting.
    fn forward(&self, request: &Value) -> bool {
        self.waiters()
            .pending
            .values()
            .any(|sender| sender.send(StreamMessage::Request(request.clone())).is_ok())
    }

    /// Fails every waiting request, and every later one, with `reason`.
    fn close(&self, reason: String) {
        let mut waiters = self.waiters();
        for (_, sender) in waiters.pending.drain() {
            let _ = sender.send(StreamMessage::Closed(reason.clone()));
        }
        waiters.closed.get_or_insert(reason);
    }
}

/// A request waiting for its response. It stops waiting when dropped.
struct Waiter<'a> {
    stream: &'a EventStream,
    key: String,
    receiver: mpsc::Receiver<StreamMessage>,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.stream.waiters().pending.remove(&self.key);
    }
}

/// The thread that reads the event stream. Dropping it stops the thread and
/// waits for it to finish.
struct ReaderThread {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ReaderThread {
    fn spawn(reader: EventStreamReader) -> Result<Self, String> {
        let (stop, stopped) = oneshot::channel();
        let server_name = reader.server_name.clone();
        let thread = std::thread::Builder::new()
            .name("mcp-sse-events".to_string())
            .spawn(move || reader.run(stopped))
            .map_err(|error| {
                format!("failed to start the SSE reader for MCP server '{server_name}': {error}")
            })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for ReaderThread {
    fn drop(&mut self) {
        // Dropping the sender wakes the reader, which drops the stream and
        // returns.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Why the event stream ended before it named an endpoint.
struct NoEndpoint {
    reason: String,
    /// The server refused to open the stream with 401.
    unauthorized: bool,
}

/// Reads the event stream on its own thread. It announces the endpoint,
/// hands each response to the request waiting for it, and fails every
/// waiting request once the stream ends.
struct EventStreamReader {
    server_name: String,
    /// The configured url, which the endpoint is resolved against.
    base: Url,
    headers: HeaderMap,
    stream: Arc<EventStream>,
    /// Where to announce the endpoint, or why there is none, until it has
    /// been announced.
    announce: Option<mpsc::Sender<Result<Url, NoEndpoint>>>,
    /// The endpoint, once announced.
    endpoint: Option<Url>,
    /// Whether the server refused to open the stream with 401.
    unauthorized: bool,
    /// How long a reply the reader POSTs itself may take.
    reply_timeout: Duration,
}

impl EventStreamReader {
    fn run(mut self, mut stop: oneshot::Receiver<()>) {
        let reason = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(self.read(&mut stop)),
            Err(error) => format!(
                "failed to start the SSE reader for MCP server '{}': {error}",
                self.server_name
            ),
        };
        if let Some(announce) = self.announce.take() {
            let _ = announce.send(Err(NoEndpoint {
                reason: reason.clone(),
                unauthorized: self.unauthorized,
            }));
        }
        self.stream.close(reason);
    }

    /// Reads the stream until it ends or the transport is dropped, and says
    /// why it stopped.
    async fn read(&mut self, stop: &mut oneshot::Receiver<()>) -> String {
        let client = crate::http::client();
        let mut response = tokio::select! {
            response = self.open(&client) => match response {
                Ok(response) => response,
                Err(reason) => return reason,
            },
            _ = &mut *stop => return MCP_SSE_EVENT_STREAM_CLOSED.to_string(),
        };
        let mut decoder = SseDecoder::new();
        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => chunk,
                _ = &mut *stop => return MCP_SSE_EVENT_STREAM_CLOSED.to_string(),
            };
            let chunk = match chunk {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return self.ended(None),
                Err(error) => return self.ended(Some(error)),
            };
            // `push` may leave events behind, after an unreadable one or the
            // events before it, so it is asked again until it has no more.
            let mut bytes: &[u8] = &chunk;
            loop {
                match decoder.push(bytes) {
                    Ok(events) if events.is_empty() => break,
                    Ok(events) => {
                        for event in events {
                            if let Err(reason) = self.handle(&event, &client) {
                                return reason;
                            }
                        }
                    }
                    // An event that cannot be read cannot be matched to a
                    // request either, so it is skipped, as the reference SDK
                    // does, and only it: the events around it are read. A
                    // request whose response it was times out.
                    Err(_) => {}
                }
                bytes = &[];
            }
            if decoder.buffered_len() > MAX_SSE_RESPONSE_BYTES {
                return format!(
                    "{MCP_SSE_EVENT_STREAM_CLOSED}: an event exceeded {MAX_SSE_RESPONSE_BYTES} bytes"
                );
            }
        }
    }

    /// Opens the event stream: a GET that must answer with one.
    async fn open(&mut self, client: &reqwest::Client) -> Result<reqwest::Response, String> {
        let refused = |why: String| {
            format!(
                "MCP server '{}' could not open its SSE event stream: {why}",
                self.server_name
            )
        };
        let mut headers = self.headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static(EVENT_STREAM));
        let response = client
            .get(self.base.clone())
            .headers(headers)
            .send()
            .await
            .map_err(|error| refused(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            self.unauthorized = status == StatusCode::UNAUTHORIZED;
            return Err(refused(status.to_string()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.to_ascii_lowercase().starts_with(EVENT_STREAM) {
            return Err(refused(format!(
                "it answered with '{content_type}' instead"
            )));
        }
        Ok(response)
    }

    /// Why the stream stopped, when the server ended it or the connection
    /// broke.
    fn ended(&self, error: Option<reqwest::Error>) -> String {
        let reason = match self.endpoint {
            Some(_) => MCP_SSE_EVENT_STREAM_CLOSED.to_string(),
            None => format!(
                "MCP server '{}' closed its SSE event stream before sending an endpoint",
                self.server_name
            ),
        };
        match error {
            Some(error) => format!("{reason}: {error}"),
            None => reason,
        }
    }

    /// Acts on one event. An error ends the stream.
    fn handle(&mut self, event: &SseEvent, client: &reqwest::Client) -> Result<(), String> {
        // Whitespace around the data and the event's name is not part of them.
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let name = event.event.as_deref().map(str::trim);
        match name.filter(|name| !name.is_empty()).unwrap_or("message") {
            "endpoint" => self.accept_endpoint(data),
            "message" => {
                if let Ok(message) = serde_json::from_str(data) {
                    self.dispatch(message, client);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Resolves the first endpoint against the configured url and announces
    /// it. It must be on the configured url's origin.
    fn accept_endpoint(&mut self, data: &str) -> Result<(), String> {
        if self.endpoint.is_some() {
            return Ok(());
        }
        let endpoint = self.base.join(data).map_err(|error| {
            format!(
                "MCP server '{}' sent an invalid endpoint '{data}': {error}",
                self.server_name
            )
        })?;
        if endpoint.origin() != self.base.origin() {
            return Err(format!(
                "MCP server '{}' sent an endpoint on another origin: {endpoint}",
                self.server_name
            ));
        }
        self.endpoint = Some(endpoint.clone());
        if let Some(announce) = self.announce.take() {
            let _ = announce.send(Ok(endpoint));
        }
        Ok(())
    }

    /// Hands a response to the request waiting for it, and answers a request
    /// from the server: a question goes to a waiting request to answer, and
    /// is turned down when none is waiting, and any other request is
    /// answered at once, as on the other transports (see
    /// [`server_request_reply`]). Notifications are skipped.
    fn dispatch(&self, message: Value, client: &reqwest::Client) {
        if message.get("method").is_none() {
            if let Some(id) = message.get("id").map(json_rpc_id_to_string) {
                self.stream.deliver(&id, message);
            }
            return;
        }
        if !is_server_request(&message) {
            return;
        }
        if !is_elicitation_create_request(&message) {
            self.reply(client, server_request_reply(&message));
        } else if !self.stream.forward(&message) {
            self.reply(client, elicitation_not_supported(&message));
        }
    }

    /// POSTs a reply to the server in the background, so the stream keeps
    /// being read meanwhile.
    fn reply(&self, client: &reqwest::Client, message: Value) {
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        let request = client
            .post(endpoint)
            .headers(self.headers.clone())
            .timeout(self.reply_timeout)
            .json(&message)
            .send();
        tokio::spawn(async move {
            let _ = request.await;
        });
    }
}
