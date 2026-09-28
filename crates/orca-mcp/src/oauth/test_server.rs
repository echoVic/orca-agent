//! A local server for OAuth tests. One port is both an MCP server, whose
//! `/mcp` endpoint takes bearer tokens, and the authorization server that
//! issues them, so one request log shows every step of a login. Every read
//! and every wait has a timeout.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::LOCATION;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

use super::OpenBrowser;

/// How long the server waits for a request, and a test for anything. Only a
/// failing test waits this long.
pub const FIXTURE_WAIT: Duration = Duration::from_secs(5);

/// The code the authorization endpoint hands out.
const CODE: &str = "code-1";

/// How the server departs from a well-behaved one.
#[derive(Clone, Debug, Default)]
pub struct OAuthTestBehavior {
    /// The authorization endpoint sends the browser back with a state of its
    /// own instead of the one it was given.
    pub wrong_state: bool,
    /// The token endpoint turns every refresh away.
    pub refuse_refresh: bool,
    /// `/mcp` turns every token away, even ones just issued.
    pub reject_every_token: bool,
    /// The 401 from `/mcp` names no resource metadata, so a client must find
    /// it at the well-known url on the server's origin.
    pub bare_challenge: bool,
    /// Tokens `/mcp` takes besides the ones the token endpoint issues.
    pub accepted_tokens: Vec<String>,
}

/// One request the server received.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    /// The path, without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.header_values(name).into_iter().next()
    }

    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// The body's form fields.
    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }

    /// The body as JSON, or null when it is not.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// The server. It stops when dropped.
pub struct OAuthTestServer {
    addr: SocketAddr,
    state: Arc<ServerState>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct ServerState {
    behavior: OAuthTestBehavior,
    requests: Mutex<Vec<RecordedRequest>>,
    /// The tokens `/mcp` takes.
    accepted: Mutex<Vec<String>>,
    /// The code challenge of the latest authorization request.
    challenge: Mutex<Option<String>>,
}

impl OAuthTestServer {
    /// Starts serving on a free local port. One thread accepts each
    /// connection and answers it by its path.
    pub fn start(behavior: OAuthTestBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the OAuth test server");
        listener
            .set_nonblocking(true)
            .expect("make the OAuth test server nonblocking");
        let addr = listener
            .local_addr()
            .expect("the OAuth test server address");
        let state = Arc::new(ServerState {
            accepted: Mutex::new(behavior.accepted_tokens.clone()),
            behavior,
            requests: Mutex::default(),
            challenge: Mutex::default(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            move || {
                let base = format!("http://{addr}");
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &state, &base),
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            }
        });
        Self {
            addr,
            state,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The url of the MCP endpoint.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.url())
    }

    /// A streamable HTTP server named `name` at the MCP endpoint.
    pub fn config(&self, name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Http,
            url: Some(self.mcp_url()),
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(5_000),
            ..Default::default()
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.state.requests).clone()
    }

    pub fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    /// Each request received: its method and path.
    pub fn trail(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }
}

impl Drop for OAuthTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A browser for login tests. On a thread of its own it GETs the
/// authorization url, follows the redirect to the callback, and sends back
/// the page the callback answers with.
pub fn test_browser() -> (OpenBrowser, mpsc::Receiver<String>) {
    let (page_sender, page) = mpsc::channel();
    let browser = move |url: &str| {
        let url = url.to_string();
        std::thread::spawn(move || {
            let Ok(client) = reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(FIXTURE_WAIT)
                .build()
            else {
                return;
            };
            let Some(callback) = client.get(&url).send().ok().and_then(|response| {
                Some(response.headers().get(LOCATION)?.to_str().ok()?.to_string())
            }) else {
                return;
            };
            if let Some(text) = client
                .get(&callback)
                .send()
                .ok()
                .and_then(|response| response.text().ok())
            {
                let _ = page_sender.send(text);
            }
        });
        Ok(())
    };
    (Box::new(browser), page)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads, records, and answers one request.
fn serve(mut stream: TcpStream, state: &ServerState, base: &str) {
    // An accepted socket may inherit the listener's nonblocking mode.
    let _ = stream.set_nonblocking(false);
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    lock(&state.requests).push(request.clone());
    let response = route(state, &request, base);
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn route(state: &ServerState, request: &RecordedRequest, base: &str) -> String {
    let protected_resource = || {
        json_response(
            200,
            &json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [format!("{base}/tenant")],
                "scopes_supported": ["mcp.read", "mcp.write"]
            }),
        )
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/mcp") => mcp(state, request, base),
        ("GET", "/.well-known/oauth-protected-resource/mcp") => protected_resource(),
        ("GET", "/.well-known/oauth-protected-resource") if state.behavior.bare_challenge => {
            protected_resource()
        }
        ("GET", "/.well-known/oauth-authorization-server/tenant") => json_response(
            200,
            &json!({
                "issuer": format!("{base}/tenant"),
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"]
            }),
        ),
        ("POST", "/register") => json_response(
            201,
            &json!({
                "client_id": "registered-client",
                "client_name": request.json()["client_name"],
                "redirect_uris": request.json()["redirect_uris"],
                "token_endpoint_auth_method": "none"
            }),
        ),
        ("GET", "/authorize") => authorize(state, request),
        ("POST", "/token") => token(state, request),
        _ => empty_response(404),
    }
}

/// The MCP endpoint. It answers requests that carry a token it takes, and
/// turns the rest away with a challenge that names its metadata.
fn mcp(state: &ServerState, request: &RecordedRequest, base: &str) -> String {
    let authorized = !state.behavior.reject_every_token
        && request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| {
                lock(&state.accepted)
                    .iter()
                    .any(|accepted| accepted == token)
            });
    if !authorized {
        let challenge = if state.behavior.bare_challenge {
            "Bearer realm=\"mcp\"".to_string()
        } else {
            format!(
                "Bearer error=\"invalid_token\", resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
            )
        };
        return format!(
            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: {challenge}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
    }
    let message = request.json();
    let Some(id) = message.get("id").cloned() else {
        // A notification.
        return empty_response(202);
    };
    let result = match message["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "oauth-fixture", "version": "1"}
        }),
        Some("tools/list") => json!({"tools": [{
            "name": "echo",
            "description": "echoes its text",
            "inputSchema": {"type": "object"}
        }]}),
        _ => json!({}),
    };
    json_response(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// The authorization endpoint. It approves at once, and sends the browser
/// back to the redirect uri with a code.
fn authorize(state: &ServerState, request: &RecordedRequest) -> String {
    let query = &request.query;
    let (Some(redirect_uri), Some(challenge), Some("S256")) = (
        query.get("redirect_uri"),
        query.get("code_challenge"),
        query.get("code_challenge_method").map(String::as_str),
    ) else {
        return empty_response(400);
    };
    let Ok(mut location) = Url::parse(redirect_uri) else {
        return empty_response(400);
    };
    *lock(&state.challenge) = Some(challenge.clone());
    let returned_state = if state.behavior.wrong_state {
        "forged-state"
    } else {
        query.get("state").map_or("", String::as_str)
    };
    location
        .query_pairs_mut()
        .append_pair("code", CODE)
        .append_pair("state", returned_state);
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

/// The token endpoint. It issues `at-1` and `rt-1` for the code, when the
/// verifier matches the challenge by S256, and `at-2` and `rt-2` for `rt-1`.
fn token(state: &ServerState, request: &RecordedRequest) -> String {
    let form = request.form();
    let field = |name: &str| form.get(name).map(String::as_str);
    let issued = match field("grant_type") {
        Some("authorization_code") => {
            let challenge = lock(&state.challenge).clone();
            let verified = field("code_verifier").is_some_and(|verifier| {
                Some(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))) == challenge
            });
            (field("code") == Some(CODE) && verified && field("resource").is_some())
                .then_some(("at-1", "rt-1"))
        }
        Some("refresh_token") if !state.behavior.refuse_refresh => {
            (field("refresh_token") == Some("rt-1")).then_some(("at-2", "rt-2"))
        }
        _ => None,
    };
    let Some((access_token, refresh_token)) = issued else {
        return json_response(400, &json!({"error": "invalid_grant"}));
    };
    lock(&state.accepted).push(access_token.to_string());
    json_response(
        200,
        &json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": refresh_token,
            "scope": "mcp.read mcp.write"
        }),
    )
}

fn json_response(status: u16, body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )
}

fn empty_response(status: u16) -> String {
    format!(
        "HTTP/1.1 {status} {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        reason(status)
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Unexpected",
    }
}

/// Reads one request without panicking: the server thread outlives the
/// assertions that would report it.
fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    stream.set_read_timeout(Some(FIXTURE_WAIT)).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split_whitespace();
        let method = request_line.next()?.to_string();
        let target = request_line.next()?;
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect::<Vec<_>>();
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = head_end + 4;
        if buffer.len() < body_start + length {
            continue;
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        return Some(RecordedRequest {
            method,
            path: path.to_string(),
            query: url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect(),
            headers,
            body: String::from_utf8_lossy(&buffer[body_start..body_start + length]).into_owned(),
        });
    }
}
