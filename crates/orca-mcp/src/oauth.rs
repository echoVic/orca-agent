//! Logging in to a remote MCP server with OAuth 2.1, as the MCP
//! authorization spec lays out:
//!
//! 1. an `initialize` sent without credentials is answered with 401, whose
//!    `WWW-Authenticate` header names the server's protected resource
//!    metadata (RFC 9728);
//! 2. that metadata names the authorization server, whose own metadata
//!    (RFC 8414, or OpenID discovery) names its endpoints;
//! 3. Orca registers itself as a public client (RFC 7591), unless the config
//!    names a client ID;
//! 4. the browser authorizes Orca with PKCE (S256), naming the server's url
//!    as the resource (RFC 8707), and comes back to a loopback callback that
//!    checks the `state`;
//! 5. the code is exchanged for tokens, which are stored for the transports
//!    to send, and to refresh with [`refresh`].
//!
//! No token is ever part of an error, and a code or token is only ever sent
//! to the endpoint named, never along a redirect.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, WWW_AUTHENTICATE};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use orca_core::config::mcp_credentials::{McpCredential, save_mcp_credential};
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

use crate::transport::{HTTP_ACCEPT, configured_headers, initialize_params, timeout_from_ms};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_server;

/// How long [`login`] waits for the browser, unless told otherwise.
pub const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// The largest metadata, registration, or token response Orca reads.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
/// The largest request head the callback reads.
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
/// How long a connection to the callback may take to send its request.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the callback checks for a connection.
const CALLBACK_POLL: Duration = Duration::from_millis(20);

/// Opens a url in the user's browser.
pub type OpenBrowser = Box<dyn FnOnce(&str) -> io::Result<()> + Send>;

/// What [`login`] needs besides the server.
pub struct McpLoginOptions {
    /// Where the tokens are stored: `$ORCA_HOME/mcp-credentials.json`.
    pub credentials_path: PathBuf,
    /// Opens the authorization url in the user's browser.
    pub open_browser: OpenBrowser,
    /// How long to wait for the browser to come back.
    pub callback_timeout: Duration,
}

impl McpLoginOptions {
    /// Options that wait [`DEFAULT_CALLBACK_TIMEOUT`] for the browser.
    pub fn new(credentials_path: PathBuf, open_browser: OpenBrowser) -> Self {
        Self {
            credentials_path,
            open_browser,
            callback_timeout: DEFAULT_CALLBACK_TIMEOUT,
        }
    }
}

/// Logs in to the remote MCP server `server` in the user's browser, and
/// stores the tokens in `options.credentials_path` under the server's name
/// and url, where its transports find them.
pub fn login(server: &McpServerConfig, options: McpLoginOptions) -> Result<(), String> {
    let name = &server.name;
    let server_url = server
        .url
        .as_deref()
        .filter(|_| server.transport != McpTransportKind::Stdio)
        .ok_or_else(|| format!("MCP server '{name}' is not a remote server"))?;
    let resource = Url::parse(server_url)
        .map_err(|error| format!("MCP server '{name}' has an invalid url: {error}"))?;
    let client = http_client(server, true)?;

    let metadata_url = discover(&client, server, &resource)?;
    let protected: ProtectedResource = get_json(&client, &metadata_url).map_err(|why| {
        format!("failed to read the protected resource metadata of MCP server '{name}' from {metadata_url}: {why}")
    })?;
    let issuer = protected
        .authorization_servers
        .as_deref()
        .and_then(<[String]>::first)
        .ok_or_else(|| format!("MCP server '{name}' names no authorization server"))?;
    let issuer = Url::parse(issuer).map_err(|_| {
        format!(
            "MCP server '{name}' names an invalid authorization server '{}'",
            printable(issuer)
        )
    })?;
    let endpoints = authorization_server(&client, server, &issuer)?;

    let callback = Callback::listen(server)?;
    let redirect_uri = callback.redirect_uri.clone();
    let client_id = match &server.oauth_client_id {
        Some(client_id) => client_id.clone(),
        None => register(
            &client,
            server,
            endpoints.registration_endpoint.as_deref(),
            &redirect_uri,
        )?,
    };
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let state = Uuid::new_v4().simple().to_string();
    let scope = protected.scope();
    let challenge = pkce_challenge(&verifier);
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("resource", server_url),
    ];
    if let Some(scope) = &scope {
        params.push(("scope", scope));
    }
    let authorization_url = web_url(&endpoints.authorization_endpoint)
        .map(|mut url| {
            url.query_pairs_mut().extend_pairs(&params);
            url
        })
        .ok_or_else(|| {
            format!("the authorization server of MCP server '{name}' has an invalid authorization endpoint")
        })?;
    (options.open_browser)(authorization_url.as_str()).map_err(|error| {
        format!("failed to open the browser to log in to MCP server '{name}': {error}")
    })?;
    let code = callback.wait_for_code(name, &state, options.callback_timeout)?;

    let tokens = request_tokens(
        server,
        &endpoints.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
            ("resource", server_url),
        ],
    )?;
    let credential = McpCredential {
        server_url: server_url.to_string(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
        token_endpoint: endpoints.token_endpoint,
        client_id,
        resource: server_url.to_string(),
        scope: tokens.scope.or(scope),
    };
    save_mcp_credential(&options.credentials_path, name, &credential)
        .map_err(|error| format!("failed to save the login for MCP server '{name}': {error}"))
}

/// Refreshes `credential`'s access token, stores the new tokens in
/// `credentials_path`, and returns them.
pub(crate) fn refresh(
    server: &McpServerConfig,
    credentials_path: &Path,
    credential: &McpCredential,
) -> Result<McpCredential, String> {
    let name = &server.name;
    let refresh_token = credential
        .refresh_token
        .as_deref()
        .ok_or_else(|| format!("MCP server '{name}' has no refresh token"))?;
    let tokens = request_tokens(
        server,
        &credential.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &credential.client_id),
            ("resource", &credential.resource),
        ],
    )?;
    let refreshed = McpCredential {
        access_token: tokens.access_token,
        // A server that does not rotate refresh tokens keeps the one it
        // issued before.
        refresh_token: tokens
            .refresh_token
            .or_else(|| credential.refresh_token.clone()),
        expires_at: tokens.expires_at,
        scope: tokens.scope.or_else(|| credential.scope.clone()),
        ..credential.clone()
    };
    save_mcp_credential(credentials_path, name, &refreshed).map_err(|error| {
        format!("failed to save the refreshed login for MCP server '{name}': {error}")
    })?;
    Ok(refreshed)
}

/// The time now, in Unix seconds.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A client that gives each request the server's startup timeout.
/// `follow_redirects` is false for requests that carry a code or a token.
fn http_client(server: &McpServerConfig, follow_redirects: bool) -> Result<Client, String> {
    let redirects = if follow_redirects {
        reqwest::redirect::Policy::default()
    } else {
        reqwest::redirect::Policy::none()
    };
    Client::builder()
        .timeout(timeout_from_ms(server.startup_timeout_ms))
        .redirect(redirects)
        .build()
        .map_err(|error| {
            format!(
                "failed to start an HTTP client for MCP server '{}': {error}",
                server.name
            )
        })
}

/// Asks the server, without credentials, where its protected resource
/// metadata is: the `resource_metadata` of the 401 it answers an
/// `initialize` POST with, or else a GET for an event stream, as a legacy
/// SSE server wants. When the 401 names none, the metadata is at the
/// well-known url on the server's origin.
fn discover(client: &Client, server: &McpServerConfig, url: &Url) -> Result<Url, String> {
    let name = &server.name;
    let mut headers = configured_headers(server)?;
    headers.remove(AUTHORIZATION);
    let unreachable =
        |error: reqwest::Error| format!("failed to reach MCP server '{name}': {error}");
    let post = client
        .post(url.clone())
        .headers(headers.clone())
        .header(ACCEPT, HTTP_ACCEPT)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": initialize_params()
        }))
        .send()
        .map_err(unreachable)?;
    let challenge = if post.status() == StatusCode::UNAUTHORIZED {
        post.headers().clone()
    } else {
        drop(post);
        let get = client
            .get(url.clone())
            .headers(headers)
            .header(ACCEPT, "text/event-stream")
            .send()
            .map_err(unreachable)?;
        if get.status() != StatusCode::UNAUTHORIZED {
            return Err(format!("MCP server '{name}' did not ask for login"));
        }
        get.headers().clone()
    };
    match resource_metadata(&challenge) {
        Some(metadata) => Url::parse(&metadata)
            .map_err(|_| format!("MCP server '{name}' named an invalid resource metadata url")),
        None => Ok(on_origin(url, "/.well-known/oauth-protected-resource")),
    }
}

/// What Orca reads of a protected resource's metadata (RFC 9728).
#[derive(Deserialize)]
struct ProtectedResource {
    #[serde(default)]
    authorization_servers: Option<Vec<String>>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

impl ProtectedResource {
    /// The scopes to ask for: every scope the resource supports.
    fn scope(&self) -> Option<String> {
        self.scopes_supported
            .as_ref()
            .filter(|scopes| !scopes.is_empty())
            .map(|scopes| scopes.join(" "))
    }
}

/// What Orca reads of an authorization server's metadata (RFC 8414).
#[derive(Deserialize)]
struct AuthorizationServer {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
}

/// Reads the metadata of the authorization server `issuer`, from where
/// RFC 8414 puts it, or else from where OpenID discovery does.
fn authorization_server(
    client: &Client,
    server: &McpServerConfig,
    issuer: &Url,
) -> Result<AuthorizationServer, String> {
    let mut failures = Vec::new();
    for url in authorization_server_metadata_urls(issuer) {
        match get_json(client, &url) {
            Ok(metadata) => return Ok(metadata),
            Err(why) => failures.push(format!("{url}: {why}")),
        }
    }
    Err(format!(
        "failed to read the authorization server metadata of MCP server '{}' ({})",
        server.name,
        failures.join("; ")
    ))
}

/// Where RFC 8414 puts an issuer's metadata, then where OpenID discovery
/// does. The well-known path goes before the path of an issuer that has one,
/// and for OpenID after it.
fn authorization_server_metadata_urls(issuer: &Url) -> [Url; 2] {
    let path = issuer.path().trim_end_matches('/');
    [
        on_origin(
            issuer,
            &format!("/.well-known/oauth-authorization-server{path}"),
        ),
        on_origin(issuer, &format!("{path}/.well-known/openid-configuration")),
    ]
}

/// Registers Orca with the authorization server as a public client
/// (RFC 7591), and returns the client ID it was given.
fn register(
    client: &Client,
    server: &McpServerConfig,
    registration_endpoint: Option<&str>,
    redirect_uri: &str,
) -> Result<String, String> {
    let name = &server.name;
    let endpoint = registration_endpoint.ok_or_else(|| {
        format!("the authorization server of MCP server '{name}' does not register clients; set oauth_client_id for the server")
    })?;
    let failed = |why: String| {
        format!(
            "failed to register Orca with the authorization server of MCP server '{name}': {why}"
        )
    };
    let response = client
        .post(endpoint)
        .header(ACCEPT, "application/json")
        .json(&json!({
            "client_name": "Orca",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .map_err(|error| failed(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(failed(format!("it answered {status}")));
    }
    #[derive(Deserialize)]
    struct Registration {
        client_id: String,
    }
    read_json::<Registration>(response)
        .map(|registration| registration.client_id)
        .filter(|client_id| !client_id.is_empty())
        .ok_or_else(|| failed("its answer names no client ID".to_string()))
}

/// The S256 code challenge for `verifier`: BASE64URL(SHA-256(verifier)),
/// without padding.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What a token endpoint issued.
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    /// When the access token expires, in Unix seconds.
    expires_at: Option<u64>,
    scope: Option<String>,
}

/// POSTs a token request and reads the tokens issued. The request is not
/// redirected, so the code or refresh token it carries goes to the endpoint
/// named and nowhere else.
fn request_tokens(
    server: &McpServerConfig,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<Tokens, String> {
    let name = &server.name;
    let failed = |why: String| format!("the token request for MCP server '{name}' failed: {why}");
    let response = http_client(server, false)?
        .post(token_endpoint)
        .header(ACCEPT, "application/json")
        .form(form)
        .send()
        .map_err(|error| failed(error.to_string()))?;
    let status = response.status();
    let body = read_json::<Value>(response);
    if !status.is_success() {
        // Only the error code is shown: the rest of the answer is the
        // server's to word.
        let code = body
            .as_ref()
            .and_then(|body| body.get("error")?.as_str())
            .map(|code| format!(" ({})", printable(code)))
            .unwrap_or_default();
        return Err(failed(format!("it answered {status}{code}")));
    }
    body.as_ref()
        .and_then(issued_tokens)
        .ok_or_else(|| failed("its answer holds no access token".to_string()))
}

fn issued_tokens(body: &Value) -> Option<Tokens> {
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
    let access_token = text("access_token").filter(|token| !token.is_empty())?;
    // Some servers send the lifetime as a string.
    let expires_in = body
        .get("expires_in")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()));
    Some(Tokens {
        access_token,
        refresh_token: text("refresh_token"),
        expires_at: expires_in.map(|seconds| unix_now().saturating_add(seconds)),
        scope: text("scope"),
    })
}

/// GETs a JSON document.
fn get_json<T: DeserializeOwned>(client: &Client, url: &Url) -> Result<T, String> {
    let response = client
        .get(url.clone())
        .header(ACCEPT, "application/json")
        .send()
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("it answered {status}"));
    }
    read_json(response).ok_or_else(|| "its answer is not the JSON expected".to_string())
}

/// Reads a JSON body of at most [`MAX_RESPONSE_BYTES`] as a `T`.
fn read_json<T: DeserializeOwned>(response: Response) -> Option<T> {
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .ok()?;
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

/// The `resource_metadata` parameter of the Bearer challenge in the
/// `WWW-Authenticate` headers.
fn resource_metadata(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|challenges| bearer_param(challenges, "resource_metadata"))
}

/// The value of the parameter `name` in the Bearer challenge of a
/// `WWW-Authenticate` value, which may hold several challenges. Scheme and
/// parameter names match in any case; values may be quoted or bare.
fn bearer_param(challenges: &str, name: &str) -> Option<String> {
    let mut rest = challenges;
    let mut in_bearer = false;
    loop {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_ascii_whitespace());
        if rest.is_empty() {
            return None;
        }
        let token_end = rest
            .find(|c: char| c == '=' || c == ',' || c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let token = &rest[..token_end];
        let after = rest[token_end..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        match after.strip_prefix('=') {
            Some(value) => {
                let (value, remaining) =
                    param_value(value.trim_start_matches(|c: char| c.is_ascii_whitespace()));
                if in_bearer && token.eq_ignore_ascii_case(name) {
                    return Some(value);
                }
                rest = remaining;
            }
            // A token not followed by `=` starts the next challenge.
            None => {
                in_bearer = token.eq_ignore_ascii_case("Bearer");
                rest = &rest[token_end..];
            }
        }
    }
}

/// Reads a parameter value, quoted or bare, and returns it with the text
/// after it.
fn param_value(text: &str) -> (String, &str) {
    let Some(quoted) = text.strip_prefix('"') else {
        let end = text
            .find(|c: char| c == ',' || c.is_ascii_whitespace())
            .unwrap_or(text.len());
        return (text[..end].to_string(), &text[end..]);
    };
    let mut value = String::new();
    let mut characters = quoted.char_indices();
    while let Some((index, character)) = characters.next() {
        match character {
            '"' => return (value, &quoted[index + 1..]),
            '\\' => value.extend(characters.next().map(|(_, escaped)| escaped)),
            character => value.push(character),
        }
    }
    (value, "")
}

/// `path` on `url`'s origin.
fn on_origin(url: &Url, path: &str) -> Url {
    let mut on_origin = url.clone();
    on_origin.set_path(path);
    on_origin.set_query(None);
    on_origin.set_fragment(None);
    on_origin
}

/// `url`, when it is an http or https url: the only kind a browser is sent
/// to.
fn web_url(url: &str) -> Option<Url> {
    Url::parse(url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// Text a server sent, fit for a terminal: without control characters, and
/// no longer than 200 characters.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect()
}

/// The loopback listener the browser comes back to. Dropping it releases
/// the port.
struct Callback {
    listener: TcpListener,
    redirect_uri: String,
}

impl Callback {
    /// Listens on 127.0.0.1 only, at the configured port or any free one.
    fn listen(server: &McpServerConfig) -> Result<Self, String> {
        let failed = |error: io::Error| {
            format!(
                "failed to listen for the browser login of MCP server '{}': {error}",
                server.name
            )
        };
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, server.oauth_callback_port.unwrap_or(0)))
                .map_err(failed)?;
        listener.set_nonblocking(true).map_err(failed)?;
        let port = listener.local_addr().map_err(failed)?.port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// Waits up to `timeout` for the browser to come back to `/callback`
    /// with a code, answering any other request with 404.
    fn wait_for_code(self, name: &str, state: &str, timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out waiting for the browser login for MCP server '{name}'"
                ));
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(outcome) = answer_callback(stream, name, state, remaining) {
                        return outcome;
                    }
                }
                // Nothing yet, or a connection that failed before it could
                // be accepted.
                Err(_) => std::thread::sleep(CALLBACK_POLL.min(remaining)),
            }
        }
    }
}

/// Answers one connection to the callback listener, and returns what the
/// browser came back with: `None` for a request that is not the callback,
/// or that never arrives.
fn answer_callback(
    mut stream: TcpStream,
    name: &str,
    state: &str,
    remaining: Duration,
) -> Option<Result<String, String>> {
    // An accepted socket may inherit the listener's nonblocking mode.
    stream.set_nonblocking(false).ok()?;
    stream
        .set_read_timeout(Some(
            remaining.clamp(Duration::from_millis(1), CALLBACK_READ_TIMEOUT),
        ))
        .ok()?;
    stream.set_write_timeout(Some(CALLBACK_READ_TIMEOUT)).ok()?;
    let (method, target) = read_request_line(&mut stream)?;
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if method != "GET" || path != "/callback" {
        respond(&mut stream, "404 Not Found", "Not found.");
        return None;
    }
    let params = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    let outcome = callback_code(name, state, &params);
    match &outcome {
        Ok(_) => respond(
            &mut stream,
            "200 OK",
            "Login complete. You can close this window.",
        ),
        Err(_) => respond(
            &mut stream,
            "400 Bad Request",
            "Login failed. Return to Orca to see why.",
        ),
    }
    Some(outcome)
}

/// The code the browser came back with, once its `state` is the one sent.
fn callback_code(
    name: &str,
    state: &str,
    params: &HashMap<String, String>,
) -> Result<String, String> {
    if params.get("state").map(String::as_str) != Some(state) {
        return Err(format!(
            "the browser login for MCP server '{name}' came back with a mismatched state"
        ));
    }
    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .map(|description| format!(": {}", printable(description)))
            .unwrap_or_default();
        return Err(format!(
            "the authorization server refused the login for MCP server '{name}' ({}){description}",
            printable(error)
        ));
    }
    params
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .ok_or_else(|| {
            format!("the browser login for MCP server '{name}' came back without a code")
        })
}

/// Reads a request's head, and returns its method and target.
fn read_request_line(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() > MAX_CALLBACK_REQUEST_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut request_line = head.lines().next()?.split_whitespace();
    Some((
        request_line.next()?.to_string(),
        request_line.next()?.to_string(),
    ))
}

/// Answers with a short page, and closes the connection.
fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body =
        format!("<!doctype html><meta charset=\"utf-8\"><title>Orca</title><p>{message}</p>\n");
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::path::Path;
    use std::time::Instant;

    use orca_core::config::mcp_credentials::load_mcp_credential;
    use serde_json::json;

    use super::test_server::{FIXTURE_WAIT, OAuthTestBehavior, OAuthTestServer, test_browser};
    use super::*;

    fn options(credentials_path: &Path, open_browser: OpenBrowser) -> McpLoginOptions {
        McpLoginOptions {
            credentials_path: credentials_path.to_path_buf(),
            open_browser,
            callback_timeout: FIXTURE_WAIT,
        }
    }

    #[test]
    fn login_discovers_registers_and_stores_tokens() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        let (browser, page) = test_browser();

        login(&server.config("docs"), options(&credentials, browser)).expect("log in");

        assert_eq!(
            server.trail(),
            [
                "POST /mcp",
                "GET /.well-known/oauth-protected-resource/mcp",
                "GET /.well-known/oauth-authorization-server/tenant",
                "POST /register",
                "GET /authorize",
                "POST /token",
            ]
        );
        let probe = &server.requests_to("/mcp")[0];
        assert_eq!(probe.json()["method"], "initialize");
        assert_eq!(probe.header("authorization"), None);

        let registration = &server.requests_to("/register")[0];
        let redirect_uri = registration.json()["redirect_uris"][0]
            .as_str()
            .expect("a redirect uri")
            .to_string();
        assert!(
            redirect_uri.starts_with("http://127.0.0.1:") && redirect_uri.ends_with("/callback"),
            "{redirect_uri}"
        );
        assert_eq!(
            registration.json(),
            json!({
                "client_name": "Orca",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none"
            })
        );

        let authorization = &server.requests_to("/authorize")[0].query;
        assert_eq!(authorization["response_type"], "code");
        assert_eq!(authorization["client_id"], "registered-client");
        assert_eq!(authorization["redirect_uri"], redirect_uri);
        assert_eq!(authorization["code_challenge_method"], "S256");
        assert_eq!(authorization["code_challenge"].len(), 43);
        assert_eq!(authorization["resource"], server.mcp_url());
        assert_eq!(authorization["scope"], "mcp.read mcp.write");
        assert!(!authorization["state"].is_empty());

        // The server issues tokens only for a verifier that matches the
        // challenge by S256.
        let exchange = server.requests_to("/token")[0].form();
        assert_eq!(exchange["grant_type"], "authorization_code");
        assert_eq!(exchange["code"], "code-1");
        assert_eq!(exchange["redirect_uri"], redirect_uri);
        assert_eq!(exchange["client_id"], "registered-client");
        assert_eq!(exchange["code_verifier"].len(), 64);
        assert_eq!(exchange["resource"], server.mcp_url());

        let credential = load_mcp_credential(&credentials, "docs", &server.mcp_url())
            .expect("read the credentials")
            .expect("a stored login");
        assert_eq!(credential.access_token, "at-1");
        assert_eq!(credential.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(credential.server_url, server.mcp_url());
        assert_eq!(credential.resource, server.mcp_url());
        assert_eq!(credential.token_endpoint, format!("{}/token", server.url()));
        assert_eq!(credential.client_id, "registered-client");
        assert_eq!(credential.scope.as_deref(), Some("mcp.read mcp.write"));
        assert!(
            credential
                .expires_at
                .is_some_and(|at| at > unix_now() + 3000)
        );
        assert!(
            page.recv_timeout(FIXTURE_WAIT)
                .expect("the callback page")
                .contains("Login complete. You can close this window.")
        );
    }

    #[test]
    fn login_uses_a_configured_client_id_without_registering() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        let mut config = server.config("docs");
        config.oauth_client_id = Some("configured-client".to_string());
        let (browser, _page) = test_browser();

        login(&config, options(&credentials, browser)).expect("log in");

        assert!(server.requests_to("/register").is_empty());
        assert_eq!(
            server.requests_to("/authorize")[0].query["client_id"],
            "configured-client"
        );
        assert_eq!(
            server.requests_to("/token")[0].form()["client_id"],
            "configured-client"
        );
        let credential = load_mcp_credential(&credentials, "docs", &server.mcp_url())
            .expect("read the credentials")
            .expect("a stored login");
        assert_eq!(credential.client_id, "configured-client");
        assert_eq!(credential.access_token, "at-1");
    }

    #[test]
    fn login_finds_the_resource_metadata_at_the_well_known_url() {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            bare_challenge: true,
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        let (browser, _page) = test_browser();

        login(&server.config("docs"), options(&credentials, browser)).expect("log in");

        assert_eq!(
            server.trail()[..3],
            [
                "POST /mcp",
                "GET /.well-known/oauth-protected-resource",
                "GET /.well-known/oauth-authorization-server/tenant",
            ]
        );
        assert!(
            load_mcp_credential(&credentials, "docs", &server.mcp_url())
                .expect("read the credentials")
                .is_some()
        );
    }

    #[test]
    fn callback_rejects_a_mismatched_state() {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            wrong_state: true,
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        let (browser, page) = test_browser();

        let error = login(&server.config("docs"), options(&credentials, browser))
            .expect_err("a callback with another state must fail");

        assert_eq!(
            error,
            "the browser login for MCP server 'docs' came back with a mismatched state"
        );
        assert!(server.requests_to("/token").is_empty());
        assert!(!credentials.exists());
        assert!(
            page.recv_timeout(FIXTURE_WAIT)
                .expect("the callback page")
                .contains("Login failed.")
        );
    }

    #[test]
    fn login_times_out_when_no_callback_arrives() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("find a free port")
            .port();
        let mut config = server.config("docs");
        config.oauth_callback_port = Some(port);
        let options = McpLoginOptions {
            credentials_path: home.path().join("mcp-credentials.json"),
            open_browser: Box::new(|_: &str| Ok(())),
            callback_timeout: Duration::from_millis(200),
        };
        let started = Instant::now();

        let error = login(&config, options).expect_err("no browser comes back");

        assert_eq!(
            error,
            "timed out waiting for the browser login for MCP server 'docs'"
        );
        assert!(started.elapsed() < FIXTURE_WAIT, "{:?}", started.elapsed());
        assert_eq!(
            server.requests_to("/register")[0].json()["redirect_uris"][0],
            format!("http://127.0.0.1:{port}/callback")
        );
        TcpListener::bind(("127.0.0.1", port)).expect("the callback port is free again");
    }

    #[test]
    fn resource_metadata_is_read_from_the_bearer_challenge() {
        for (challenges, expected) in [
            (
                r#"Bearer resource_metadata="https://a.example/meta""#,
                Some("https://a.example/meta"),
            ),
            (
                r#"bearer realm="mcp", RESOURCE_METADATA=https://a.example/meta"#,
                Some("https://a.example/meta"),
            ),
            (
                r#"Basic realm="x", Bearer error="invalid_token", resource_metadata="https://a.example/m\"q""#,
                Some(r#"https://a.example/m"q"#),
            ),
            (r#"Basic resource_metadata="https://b.example/meta""#, None),
            ("Bearer", None),
            ("", None),
        ] {
            assert_eq!(
                bearer_param(challenges, "resource_metadata").as_deref(),
                expected,
                "{challenges}"
            );
        }
    }

    #[test]
    fn authorization_server_metadata_is_found_where_rfc_8414_then_openid_put_it() {
        let urls = |issuer: &str| {
            authorization_server_metadata_urls(&Url::parse(issuer).expect("an issuer url"))
                .map(String::from)
        };
        assert_eq!(
            urls("https://as.example/t1"),
            [
                "https://as.example/.well-known/oauth-authorization-server/t1",
                "https://as.example/t1/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            urls("https://as.example"),
            [
                "https://as.example/.well-known/oauth-authorization-server",
                "https://as.example/.well-known/openid-configuration",
            ]
        );
    }
}
