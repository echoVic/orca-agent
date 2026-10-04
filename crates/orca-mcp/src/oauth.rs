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
//! Along the way the metadata must be for this server (RFC 9728 §3.3) and
//! for the issuer asked (RFC 8414 §3.3); the authorization server must take
//! S256 when it lists PKCE methods, and use https for its metadata and
//! endpoints unless it is on this machine; and the tokens must be Bearer
//! tokens.
//!
//! No token is ever part of an error, and a code or token is only ever sent
//! to the endpoint named, never along a redirect. Metadata may come after a
//! redirect, but never after one from https to plain http.

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
use url::{Host, Url};
use uuid::Uuid;

use orca_core::config::mcp_credentials::{
    LockedMcpCredentials, McpCredential, save_mcp_credential,
};
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
/// A token this close to expiring is refreshed before it is sent.
const REFRESH_MARGIN_SECS: u64 = 60;

/// Opens a url in the user's browser.
pub type OpenBrowser = Box<dyn FnOnce(&str) -> io::Result<()> + Send>;

/// What [`login`] needs besides the server.
pub struct McpLoginOptions {
    /// Where the tokens are stored: `$ORCA_HOME/mcp-credentials.json`.
    pub credentials_path: PathBuf,
    /// Opens the authorization url in the user's browser. When it fails,
    /// the login still waits for the callback: the caller also prints the
    /// url, for the user to open by hand.
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
    // The metadata must be for this server (RFC 9728 §3.3), so that a
    // server cannot have Orca log in to another resource for it.
    match protected.resource.as_deref() {
        Some(covered) if resource_covers(covered, &resource) => {}
        Some(other) => {
            return Err(format!(
                "the protected resource metadata of MCP server '{name}' is for another resource: '{}'",
                printable(other)
            ));
        }
        None => {
            return Err(format!(
                "the protected resource metadata of MCP server '{name}' names no resource"
            ));
        }
    }
    let issuer = protected
        .authorization_servers
        .as_deref()
        .and_then(<[String]>::first)
        .ok_or_else(|| format!("MCP server '{name}' names no authorization server"))?;
    let issuer_url = oauth_url(issuer).ok_or_else(|| {
        format!(
            "the authorization server '{}' of MCP server '{name}' must use https",
            printable(issuer)
        )
    })?;
    let endpoints = authorization_server(&client, server, issuer, &issuer_url)?;
    if endpoints
        .code_challenge_methods_supported
        .as_ref()
        .is_some_and(|methods| !methods.iter().any(|method| method == "S256"))
    {
        return Err(format!(
            "the authorization server of MCP server '{name}' does not support PKCE with S256"
        ));
    }
    let insecure = |endpoint: &str| {
        format!("the authorization server of MCP server '{name}' must use https for its {endpoint}")
    };
    let mut authorization_url = oauth_url(&endpoints.authorization_endpoint)
        .ok_or_else(|| insecure("authorization endpoint"))?;
    oauth_url(&endpoints.token_endpoint).ok_or_else(|| insecure("token endpoint"))?;

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
    authorization_url.query_pairs_mut().extend_pairs(&params);
    // The url is also printed for the user to open by hand, so a browser
    // that does not open is no reason to stop waiting.
    let _ = (options.open_browser)(authorization_url.as_str());
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

/// Replaces `credential`, the login in use, with a fresh one, and returns
/// it.
///
/// The credentials file stays locked from the read of the stored login to
/// the save of the new one, so a logout, a login, or another Orca's refresh
/// waits for this one, and this one sees theirs:
/// - a login no longer stored has been logged out of, and is neither
///   refreshed nor stored again;
/// - a stored token that has replaced `credential`'s, and is not about to
///   expire, is used as it is, without a request;
/// - otherwise the stored refresh token, the newest, is spent, and the new
///   tokens are stored.
pub(crate) fn refresh(
    server: &McpServerConfig,
    credentials_path: &Path,
    credential: &McpCredential,
) -> Result<McpCredential, String> {
    let name = &server.name;
    let store = LockedMcpCredentials::lock(credentials_path)
        .map_err(|error| format!("failed to lock the MCP credentials: {error}"))?;
    let stored = store
        .load(name, &credential.server_url)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("MCP server '{name}' is no longer logged in"))?;
    if stored.access_token != credential.access_token && !expires_soon(&stored) {
        return Ok(stored);
    }
    let refresh_token = stored
        .refresh_token
        .as_deref()
        .ok_or_else(|| format!("MCP server '{name}' has no refresh token"))?;
    let tokens = request_tokens(
        server,
        &stored.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &stored.client_id),
            ("resource", &stored.resource),
        ],
    )?;
    let refreshed = McpCredential {
        access_token: tokens.access_token,
        // A server that does not rotate refresh tokens keeps the one it
        // issued before.
        refresh_token: tokens.refresh_token.or(stored.refresh_token),
        expires_at: tokens.expires_at,
        scope: tokens.scope.or(stored.scope),
        ..stored
    };
    store.save(name, &refreshed).map_err(|error| {
        format!("failed to save the refreshed login for MCP server '{name}': {error}")
    })?;
    Ok(refreshed)
}

/// Whether `credential`'s access token expires within the next minute.
pub(crate) fn expires_soon(credential: &McpCredential) -> bool {
    credential
        .expires_at
        .is_some_and(|expires_at| expires_at <= unix_now().saturating_add(REFRESH_MARGIN_SECS))
}

/// The time now, in Unix seconds.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A client that gives each request the server's startup timeout.
/// `follow_redirects` is false for requests that carry a code or a token,
/// which are never redirected. The others, the metadata requests among them,
/// follow redirects as [`metadata_redirect`] allows.
fn http_client(server: &McpServerConfig, follow_redirects: bool) -> Result<Client, String> {
    let redirects = if follow_redirects {
        reqwest::redirect::Policy::custom(|attempt| {
            match metadata_redirect(attempt.previous(), attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(why) => attempt.error(why),
            }
        })
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

/// The most redirects a metadata request follows: as many as reqwest
/// follows by default.
const MAX_METADATA_REDIRECTS: usize = 10;

/// Whether a metadata request may follow the redirect to `next`, after the
/// urls in `previous`, the first of which it was sent to. It follows up to
/// [`MAX_METADATA_REDIRECTS`] of them, and none that
/// [`allows_metadata_redirect`] turns down. The error says why not.
fn metadata_redirect(previous: &[Url], next: &Url) -> Result<(), &'static str> {
    if previous
        .last()
        .is_some_and(|previous| !allows_metadata_redirect(previous, next))
    {
        return Err("a redirect from https to http is not followed");
    }
    // The first url is the one the request was sent to, not a redirect.
    if previous.len() > MAX_METADATA_REDIRECTS {
        return Err("too many redirects");
    }
    Ok(())
}

/// Whether a metadata request may follow a redirect from `previous` to
/// `next`: never from https to plain http, where anyone on the way could
/// read the metadata or change it, and with it where the login goes.
fn allows_metadata_redirect(previous: &Url, next: &Url) -> bool {
    previous.scheme() != "https" || next.scheme() == "https"
}

/// What went wrong with a request, with why a redirect was not followed:
/// reqwest names only the url.
fn request_error(error: &reqwest::Error) -> String {
    match std::error::Error::source(error) {
        Some(why) if error.is_redirect() => format!("{error}: {why}"),
        _ => error.to_string(),
    }
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
    let unreachable = |error: reqwest::Error| {
        format!(
            "failed to reach MCP server '{name}': {}",
            request_error(&error)
        )
    };
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
    resource: Option<String>,
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
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

/// Reads the metadata of the authorization server `issuer`, from where
/// RFC 8414 puts it, or else from where OpenID discovery does. Metadata for
/// another issuer is refused (RFC 8414 §3.3).
fn authorization_server(
    client: &Client,
    server: &McpServerConfig,
    issuer: &str,
    issuer_url: &Url,
) -> Result<AuthorizationServer, String> {
    let mut failures = Vec::new();
    for url in authorization_server_metadata_urls(issuer_url) {
        match get_json::<AuthorizationServer>(client, &url) {
            Ok(metadata) if metadata.issuer == issuer => return Ok(metadata),
            Ok(metadata) => {
                return Err(format!(
                    "the authorization server metadata of MCP server '{}' is for issuer '{}', not '{}'",
                    server.name,
                    printable(&metadata.issuer),
                    printable(issuer)
                ));
            }
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
    let endpoint = oauth_url(endpoint).ok_or_else(|| {
        format!("the authorization server of MCP server '{name}' must use https for its registration endpoint")
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
        .map_err(|error| failed(request_error(&error)))?;
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
    let endpoint = oauth_url(token_endpoint).ok_or_else(|| {
        format!(
            "the authorization server of MCP server '{name}' must use https for its token endpoint"
        )
    })?;
    let failed = |why: String| format!("the token request for MCP server '{name}' failed: {why}");
    let response = http_client(server, false)?
        .post(endpoint)
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
    let body = body.ok_or_else(|| failed("its answer is not JSON".to_string()))?;
    issued_tokens(&body).map_err(failed)
}

/// The tokens in a token response, or why there are none to use. Only
/// Bearer tokens are, since that is how the transports send them.
fn issued_tokens(body: &Value) -> Result<Tokens, String> {
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
    let access_token = text("access_token")
        .filter(|token| !token.is_empty())
        .ok_or("its answer holds no access token")?;
    match text("token_type") {
        Some(token_type) if token_type.eq_ignore_ascii_case("Bearer") => {}
        Some(token_type) => {
            return Err(format!(
                "it issued a '{}' token, not a Bearer token",
                printable(&token_type)
            ));
        }
        None => return Err("its answer names no token type".to_string()),
    }
    // Some servers send the lifetime as a string.
    let expires_in = body
        .get("expires_in")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()));
    Ok(Tokens {
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
        .map_err(|error| request_error(&error))?;
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

/// `url`, when OAuth may use it: over https, or over plain http only to
/// this machine, as the MCP authorization spec requires of authorization
/// server endpoints.
fn oauth_url(url: &str) -> Option<Url> {
    let url = Url::parse(url).ok()?;
    let on_this_machine = match url.host()? {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    };
    (url.scheme() == "https" || (url.scheme() == "http" && on_this_machine)).then_some(url)
}

/// Whether the protected resource `resource` covers the server at
/// `server_url`: it is on the same origin, and the server's url starts with
/// it (RFC 9728 §3.3, which asks for the same url, loosened to a prefix).
fn resource_covers(resource: &str, server_url: &Url) -> bool {
    Url::parse(resource).is_ok_and(|resource| {
        resource.origin() == server_url.origin()
            && server_url
                .as_str()
                .starts_with(resource.as_str().trim_end_matches('/'))
    })
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
    /// with the `state` sent and a code. Any other request is answered with
    /// 404, and a callback with another state with an error page; the wait
    /// goes on after both.
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
/// browser came back with: `None` for a request that is not the callback
/// for this login, or that never arrives.
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
    // Another state means another login, such as an old browser tab's, or a
    // page that forged it: it is turned away, and this login waits on.
    if params.get("state").map(String::as_str) != Some(state) {
        respond(
            &mut stream,
            "400 Bad Request",
            "This is not the login Orca is waiting for. Finish the login Orca opened most recently.",
        );
        return None;
    }
    let outcome = callback_code(name, &params);
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

/// The code a callback with the `state` sent came back with, or why the
/// login failed.
fn callback_code(name: &str, params: &HashMap<String, String>) -> Result<String, String> {
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

    /// A browser the authorization server sends back to the callback with a
    /// state of its own (`OAuthTestBehavior::wrong_state`), which then comes
    /// back again with the state Orca sent. It hands back both pages.
    fn browser_with_a_stray_callback() -> (OpenBrowser, std::sync::mpsc::Receiver<String>) {
        let (page_sender, pages) = std::sync::mpsc::channel();
        let browser = move |url: &str| {
            let authorization = url::Url::parse(url).map_err(io::Error::other)?;
            let sent_state = authorization
                .query_pairs()
                .find(|(name, _)| name == "state")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            std::thread::spawn(move || {
                let Ok(client) = Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(FIXTURE_WAIT)
                    .build()
                else {
                    return;
                };
                let Some(stray) =
                    client
                        .get(authorization.as_str())
                        .send()
                        .ok()
                        .and_then(|response| {
                            let location = response.headers().get(reqwest::header::LOCATION)?;
                            url::Url::parse(location.to_str().ok()?).ok()
                        })
                else {
                    return;
                };
                let code = stray
                    .query_pairs()
                    .find(|(name, _)| name == "code")
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                let mut right = stray.clone();
                right
                    .query_pairs_mut()
                    .clear()
                    .append_pair("code", &code)
                    .append_pair("state", &sent_state);
                for callback in [stray, right] {
                    let page = client
                        .get(callback.as_str())
                        .send()
                        .and_then(Response::text)
                        .unwrap_or_default();
                    let _ = page_sender.send(page);
                }
            });
            Ok(())
        };
        (Box::new(browser), pages)
    }

    #[test]
    fn a_callback_with_another_state_is_turned_away_and_the_login_waits_on() {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            wrong_state: true,
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        let (browser, pages) = browser_with_a_stray_callback();

        login(&server.config("docs"), options(&credentials, browser))
            .expect("the callback with the state Orca sent logs in");

        let stray = pages.recv_timeout(FIXTURE_WAIT).expect("the stray page");
        assert!(
            stray.contains("This is not the login Orca is waiting for"),
            "{stray}"
        );
        let right = pages.recv_timeout(FIXTURE_WAIT).expect("the login page");
        assert!(right.contains("Login complete."), "{right}");
        assert_eq!(server.requests_to("/token").len(), 1);
        assert!(
            load_mcp_credential(&credentials, "docs", &server.mcp_url())
                .expect("read the credentials")
                .is_some()
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
    fn login_keeps_waiting_when_the_browser_does_not_open() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        // No browser opens, and the user opens the printed url by hand.
        let (by_hand, _page) = test_browser();
        let no_browser: OpenBrowser = Box::new(move |url: &str| {
            by_hand(url)?;
            Err(io::Error::other("no browser"))
        });

        login(&server.config("docs"), options(&credentials, no_browser)).expect("log in by hand");

        assert!(
            load_mcp_credential(&credentials, "docs", &server.mcp_url())
                .expect("read the credentials")
                .is_some()
        );
    }

    /// Logs in to a server that behaves as told, and returns its error.
    fn failed_login(behavior: OAuthTestBehavior) -> (OAuthTestServer, String) {
        let server = OAuthTestServer::start(behavior);
        let home = tempfile::tempdir().expect("temp dir");
        let (browser, _page) = test_browser();
        let error = login(
            &server.config("docs"),
            options(&home.path().join("mcp-credentials.json"), browser),
        )
        .expect_err("the login must fail");
        (server, error)
    }

    #[test]
    fn login_refuses_metadata_for_another_issuer() {
        let (server, error) = failed_login(OAuthTestBehavior {
            wrong_issuer: true,
            ..Default::default()
        });

        assert_eq!(
            error,
            format!(
                "the authorization server metadata of MCP server 'docs' is for issuer '{0}/elsewhere', not '{0}/tenant'",
                server.url()
            )
        );
        assert_eq!(server.trail().len(), 3, "{:?}", server.trail());
    }

    #[test]
    fn login_refuses_metadata_for_another_resource() {
        let (server, error) = failed_login(OAuthTestBehavior {
            wrong_resource: true,
            ..Default::default()
        });

        assert_eq!(
            error,
            format!(
                "the protected resource metadata of MCP server 'docs' is for another resource: '{}/elsewhere'",
                server.url()
            )
        );
        assert_eq!(server.trail().len(), 2, "{:?}", server.trail());
    }

    #[test]
    fn login_refuses_an_insecure_token_endpoint() {
        let (server, error) = failed_login(OAuthTestBehavior {
            insecure_token_endpoint: true,
            ..Default::default()
        });

        assert_eq!(
            error,
            "the authorization server of MCP server 'docs' must use https for its token endpoint"
        );
        assert_eq!(server.trail().len(), 3, "{:?}", server.trail());
    }

    #[test]
    fn login_refuses_an_authorization_server_without_s256() {
        let (server, error) = failed_login(OAuthTestBehavior {
            plain_pkce_only: true,
            ..Default::default()
        });

        assert_eq!(
            error,
            "the authorization server of MCP server 'docs' does not support PKCE with S256"
        );
        assert_eq!(server.trail().len(), 3, "{:?}", server.trail());
    }

    #[test]
    fn login_refuses_a_token_that_is_not_a_bearer_token() {
        let (_server, error) = failed_login(OAuthTestBehavior {
            token_type: Some("mac".to_string()),
            ..Default::default()
        });
        assert_eq!(
            error,
            "the token request for MCP server 'docs' failed: it issued a 'mac' token, not a Bearer token"
        );
        assert!(
            !error.contains("at-1") && !error.contains("rt-1"),
            "{error}"
        );

        // The token type is matched in any case.
        let server = OAuthTestServer::start(OAuthTestBehavior {
            token_type: Some("bearer".to_string()),
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let (browser, _page) = test_browser();
        login(
            &server.config("docs"),
            options(&home.path().join("mcp-credentials.json"), browser),
        )
        .expect("a lower-case bearer token is a Bearer token");
    }

    #[test]
    fn oauth_urls_use_https_unless_on_this_machine() {
        for (url, allowed) in [
            ("https://auth.example/token", true),
            ("http://127.0.0.1:8080/token", true),
            ("http://[::1]:8080/token", true),
            ("http://localhost/token", true),
            ("http://LocalHost:9/token", true),
            ("http://auth.example/token", false),
            ("http://127.0.0.1.example/token", false),
            ("ftp://127.0.0.1/token", false),
            ("not a url", false),
        ] {
            assert_eq!(oauth_url(url).is_some(), allowed, "{url}");
        }
    }

    #[test]
    fn metadata_redirects_stop_after_10_and_at_a_drop_to_http() {
        let url = |url: &str| Url::parse(url).expect("a url");
        let sent_to = |count: usize| vec![url("https://a/meta"); count];

        // The tenth redirect is followed, and not the eleventh.
        assert_eq!(
            metadata_redirect(&sent_to(10), &url("https://b/meta")),
            Ok(())
        );
        assert_eq!(
            metadata_redirect(&sent_to(11), &url("https://b/meta")),
            Err("too many redirects")
        );
        assert_eq!(
            metadata_redirect(&sent_to(1), &url("http://b/meta")),
            Err("a redirect from https to http is not followed")
        );
    }

    #[test]
    fn metadata_requests_follow_10_redirects_and_no_more() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let config = server.config("docs");
        let metadata = http_client(&config, true).expect("a client for metadata");
        let hops =
            |count: usize| Url::parse(&format!("{}/hops/{count}", server.url())).expect("a url");

        let followed: Value = get_json(&metadata, &hops(10)).expect("ten redirects are followed");
        let error = get_json::<Value>(&metadata, &hops(11)).expect_err("an eleventh is not");

        assert_eq!(followed, json!({}));
        // reqwest names only the url the request was sent to; why the
        // redirect was not followed comes after it.
        assert_eq!(
            error,
            format!(
                "error following redirect for url ({}): too many redirects",
                hops(11)
            )
        );
        let sent = server
            .requests()
            .iter()
            .filter(|request| request.path.starts_with("/hops/"))
            .count();
        assert_eq!(sent, 11 + 11);
        // A request that carries a code or a token follows no redirect.
        let tokens = http_client(&config, false).expect("a client for tokens");
        let answer = tokens.get(hops(1)).send().expect("an answer");
        assert_eq!(answer.status(), StatusCode::FOUND);
    }

    #[test]
    fn metadata_redirects_never_drop_to_http() {
        let url = |url: &str| Url::parse(url).expect("a url");

        assert!(!allows_metadata_redirect(
            &url("https://a"),
            &url("http://b")
        ));
        assert!(allows_metadata_redirect(
            &url("https://a"),
            &url("https://b")
        ));
        assert!(allows_metadata_redirect(
            &url("http://127.0.0.1"),
            &url("http://127.0.0.1:1")
        ));
    }

    #[test]
    fn a_protected_resource_must_cover_the_server_url() {
        let server_url = Url::parse("https://mcp.example/v1/mcp").expect("a server url");
        for (resource, covers) in [
            ("https://mcp.example/v1/mcp", true),
            ("https://mcp.example/v1/mcp/", true),
            ("https://mcp.example/v1", true),
            ("https://mcp.example", true),
            ("https://MCP.example/v1/mcp", true),
            ("https://mcp.example/v2", false),
            ("https://other.example/v1/mcp", false),
            ("http://mcp.example/v1/mcp", false),
            ("https://mcp.example:8443/v1/mcp", false),
            ("not a url", false),
        ] {
            assert_eq!(resource_covers(resource, &server_url), covers, "{resource}");
        }
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
