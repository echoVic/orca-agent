//! A local server for OAuth tests. One port is both an MCP server, whose
//! `/mcp` endpoint takes bearer tokens, and the authorization server that
//! issues them, so one request log shows every step of a login. Every read
//! and every wait has a timeout.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    /// The token endpoint holds each request until the gate is released.
    pub hold_token_requests: Option<TokenGate>,
    /// The authorization server metadata names another issuer.
    pub wrong_issuer: bool,
    /// The protected resource metadata names another resource.
    pub wrong_resource: bool,
    /// The authorization server metadata names a token endpoint on another
    /// machine, over plain http.
    pub insecure_token_endpoint: bool,
    /// The authorization server supports only the `plain` PKCE method.
    pub plain_pkce_only: bool,
    /// The token type the token endpoint issues, instead of `Bearer`.
    pub token_type: Option<String>,
    /// `/mcp` declares prompts, listing `review`, and resources, besides
    /// its tool.
    pub offers_prompts_and_resources: bool,
    /// Cancels the login when a request to the path comes in, before it is
    /// answered: as a user who cancels it while that step runs.
    pub cancel_on: Option<(String, super::McpLoginCancel)>,
    /// Holds each request to the path until the gate is released, once it is
    /// recorded and before it is answered: as a server that is slow to answer
    /// that step. The server answers one request at a time, so it answers
    /// no other meanwhile.
    pub hold_path: Option<(String, TokenGate)>,
}

/// Holds requests until released, so that a test can act while one is in
/// flight. A held request goes on after [`FIXTURE_WAIT`] regardless.
#[derive(Clone, Debug, Default)]
pub struct TokenGate(Arc<(Mutex<bool>, Condvar)>);

impl TokenGate {
    pub fn release(&self) {
        let (released, changed) = &*self.0;
        *lock(released) = true;
        changed.notify_all();
    }

    fn wait(&self) {
        let (released, changed) = &*self.0;
        let deadline = Instant::now() + FIXTURE_WAIT;
        let mut guard = lock(released);
        while !*guard {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            guard = changed
                .wait_timeout(guard, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
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

    /// Waits up to [`FIXTURE_WAIT`] for a request to `path`, and says
    /// whether one arrived.
    pub fn wait_for_request(&self, path: &str) -> bool {
        let deadline = Instant::now() + FIXTURE_WAIT;
        while self.requests_to(path).is_empty() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Stops taking every token `/mcp` has taken so far, as a server does
    /// once a login is revoked.
    pub fn revoke_tokens(&self) {
        lock(&self.state.accepted).clear();
    }

    /// Takes `token` from now on, as a server does once the user logs in
    /// again.
    pub fn accept_token(&self, token: &str) {
        lock(&self.state.accepted).push(token.to_string());
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
    if let Some((path, cancel)) = &state.behavior.cancel_on
        && request.path == *path
    {
        cancel.cancel();
    }
    if let Some((path, gate)) = &state.behavior.hold_path
        && request.path == *path
    {
        gate.wait();
    }
    let response = route(state, &request, base);
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn route(state: &ServerState, request: &RecordedRequest, base: &str) -> String {
    let behavior = &state.behavior;
    let protected_resource = || {
        let resource = if behavior.wrong_resource {
            format!("{base}/elsewhere")
        } else {
            format!("{base}/mcp")
        };
        json_response(
            200,
            &json!({
                "resource": resource,
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
                "issuer": if behavior.wrong_issuer {
                    format!("{base}/elsewhere")
                } else {
                    format!("{base}/tenant")
                },
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": if behavior.insecure_token_endpoint {
                    "http://auth.example/token".to_string()
                } else {
                    format!("{base}/token")
                },
                "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported":
                    if behavior.plain_pkce_only { ["plain"] } else { ["S256"] },
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
        ("GET", path) if path.starts_with("/hops/") => hops(path),
        _ => empty_response(404),
    }
}

/// `GET /hops/<n>`: a redirect to `/hops/<n - 1>`, and at `/hops/0` an empty
/// JSON object, so that a request to `/hops/<n>` is redirected `n` times.
fn hops(path: &str) -> String {
    match path.trim_start_matches("/hops/").parse::<usize>() {
        Ok(0) => json_response(200, &json!({})),
        Ok(hops) => format!(
            "HTTP/1.1 302 Found\r\nlocation: /hops/{}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            hops - 1
        ),
        Err(_) => empty_response(404),
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
    let capabilities = if state.behavior.offers_prompts_and_resources {
        json!({"tools": {}, "prompts": {}, "resources": {}})
    } else {
        json!({"tools": {}})
    };
    let result = match message["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": capabilities,
            "serverInfo": {"name": "oauth-fixture", "version": "1"}
        }),
        Some("tools/list") => json!({"tools": [{
            "name": "echo",
            "description": "echoes its text",
            "inputSchema": {"type": "object"}
        }]}),
        Some("prompts/list") => json!({"prompts": [{"name": "review"}]}),
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
    if let Some(gate) = &state.behavior.hold_token_requests {
        gate.wait();
    }
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
            "token_type": state.behavior.token_type.as_deref().unwrap_or("Bearer"),
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
