//! How a remote MCP transport authenticates its requests, in this order:
//!
//! - a configured `Authorization` header is sent as it is;
//! - otherwise `bearer_token_env_var` names the variable that holds a bearer
//!   token;
//! - otherwise the OAuth credential `orca mcp login` stored for the server
//!   and its url is sent. A token that is about to expire, or that the server
//!   turns away with 401, is refreshed, at most once for each request.
//!
//! When the server asks for a login none of these can give, the request
//! fails with an error that starts with [`MCP_AUTH_REQUIRED`]. Tokens are
//! never part of an error, and their headers are marked sensitive.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

use orca_core::config::mcp_credentials::{McpCredential, load_mcp_credential};
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

use crate::oauth::{self, expires_soon};
use crate::transport::configured_headers;

/// What an error starts with when a remote MCP server needs the user to log
/// in.
pub const MCP_AUTH_REQUIRED: &str = "MCP server requires login";

/// Whether `error` says the server needs the user to log in.
pub fn is_auth_required(error: &str) -> bool {
    error.starts_with(MCP_AUTH_REQUIRED)
}

/// How a server authenticates, decided from its config alone, in the order
/// its transports try. `orca mcp` and the TUI's `/mcp` panel both ask it
/// whether a server logs in with OAuth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpAuthKind {
    /// A stdio server: nothing is sent over a network to authenticate.
    Stdio,
    /// A configured `Authorization` header, matched case-insensitively.
    StaticHeader,
    /// `bearer_token_env_var`, carrying the environment variable's name.
    BearerEnvVar(String),
    /// Neither of the above, so OAuth is this server's only option.
    OAuth,
}

impl McpAuthKind {
    /// How `server` authenticates.
    pub fn of(server: &McpServerConfig) -> Self {
        if server.transport == McpTransportKind::Stdio {
            Self::Stdio
        } else if server
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
        {
            Self::StaticHeader
        } else if let Some(variable) = &server.bearer_token_env_var {
            Self::BearerEnvVar(variable.clone())
        } else {
            Self::OAuth
        }
    }
}

/// How requests to one remote server authenticate. Every request to the
/// server shares it, so a refreshed token serves them all.
pub(crate) struct RemoteAuth {
    server_name: String,
    source: AuthSource,
}

enum AuthSource {
    /// The configured headers carry `Authorization`.
    Configured,
    /// The bearer token from `bearer_token_env_var`.
    Bearer(HeaderValue),
    /// The OAuth credential stored for the server.
    Stored(Box<StoredLogin>),
    /// Nothing to authenticate with.
    None,
}

/// A stored OAuth credential, and what refreshing it takes.
struct StoredLogin {
    config: McpServerConfig,
    credentials_path: PathBuf,
    credential: Mutex<McpCredential>,
    /// Held while the token is refreshed, so that requests that find it
    /// stale at the same time refresh it once.
    refreshing: Mutex<()>,
}

/// The auth one request goes out with.
pub(crate) struct AuthAttempt {
    /// The `Authorization` header Orca adds, if it adds one.
    header: Option<HeaderValue>,
    /// Whether the token was refreshed for this request.
    refreshed: bool,
}

impl AuthAttempt {
    /// Adds the attempt's `Authorization` header, if it has one, to
    /// `headers`.
    pub(crate) fn apply(&self, headers: &mut HeaderMap) {
        if let Some(header) = &self.header {
            headers.insert(AUTHORIZATION, header.clone());
        }
    }
}

impl RemoteAuth {
    /// Works out how requests to `config`'s server authenticate. A stored
    /// login is read from `credentials_path`. A missing bearer token
    /// variable is an error.
    pub(crate) fn resolve(
        config: &McpServerConfig,
        credentials_path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let source = if configured_headers(config)?.contains_key(AUTHORIZATION) {
            AuthSource::Configured
        } else if let Some(variable) = &config.bearer_token_env_var {
            AuthSource::Bearer(bearer_from_env(&config.name, variable)?)
        } else {
            stored_login(config, credentials_path)
        };
        Ok(Self {
            server_name: config.name.clone(),
            source,
        })
    }

    /// The auth a new request goes out with. A stored token about to expire
    /// is refreshed first; when that fails, the user must log in again.
    pub(crate) fn begin(&self) -> Result<AuthAttempt, String> {
        let AuthSource::Stored(login) = &self.source else {
            return Ok(self.current());
        };
        let credential = login.credential();
        let header =
            bearer_header(&credential.access_token).ok_or_else(|| self.login_required())?;
        if credential.refresh_token.is_none() || !expires_soon(&credential) {
            return Ok(AuthAttempt {
                header: Some(header),
                refreshed: false,
            });
        }
        let header = login.refresh(&header).map_err(|_| self.login_required())?;
        Ok(AuthAttempt {
            header: Some(header),
            refreshed: true,
        })
    }

    /// The auth as it stands, never refreshed: for a request that cannot
    /// wait on a refresh.
    pub(crate) fn current(&self) -> AuthAttempt {
        let header = match &self.source {
            AuthSource::Configured | AuthSource::None => None,
            AuthSource::Bearer(header) => Some(header.clone()),
            AuthSource::Stored(login) => bearer_header(&login.credential().access_token),
        };
        AuthAttempt {
            header,
            refreshed: false,
        }
    }

    /// What to do once the server has answered `attempt` with 401: send the
    /// request again with the attempt returned, after refreshing the stored
    /// token; let the 401 stand (`None`), as for a token the config gives;
    /// or fail, because the user must log in.
    pub(crate) fn after_unauthorized(
        &self,
        attempt: &AuthAttempt,
    ) -> Result<Option<AuthAttempt>, String> {
        let login = match &self.source {
            AuthSource::Configured | AuthSource::Bearer(_) => return Ok(None),
            AuthSource::None => return Err(self.login_required()),
            AuthSource::Stored(login) => login,
        };
        let used = attempt
            .header
            .as_ref()
            .filter(|_| !attempt.refreshed)
            .ok_or_else(|| self.login_required())?;
        let header = login.refresh(used).map_err(|_| self.login_required())?;
        Ok(Some(AuthAttempt {
            header: Some(header),
            refreshed: true,
        }))
    }

    fn login_required(&self) -> String {
        format!(
            "{MCP_AUTH_REQUIRED}: run 'orca mcp login {}'",
            self.server_name
        )
    }
}

impl StoredLogin {
    fn credential(&self) -> McpCredential {
        lock(&self.credential).clone()
    }

    /// Replaces the token `used` carried, and returns the new one's header.
    /// When another request here has already replaced it, the new one is
    /// used as it is. Otherwise [`oauth::refresh`] replaces it, under the
    /// credentials file's lock, seeing any change another Orca or the user
    /// has made to the stored login.
    fn refresh(&self, used: &HeaderValue) -> Result<HeaderValue, String> {
        let _refreshing = lock(&self.refreshing);
        let current = self.credential();
        let current_header = bearer_header(&current.access_token)
            .ok_or("the access token is not a valid header value")?;
        if current_header != *used {
            return Ok(current_header);
        }
        let replaced = oauth::refresh(&self.config, &self.credentials_path, &current)?;
        let header = bearer_header(&replaced.access_token)
            .ok_or("the new access token is not a valid header value")?;
        *lock(&self.credential) = replaced;
        Ok(header)
    }
}

/// The login stored for `config`'s server and url. A credentials file that
/// cannot be read counts as holding none: `orca mcp login` reports what is
/// wrong with it when it goes to save.
fn stored_login(config: &McpServerConfig, credentials_path: Option<PathBuf>) -> AuthSource {
    let (Some(credentials_path), Some(url)) = (credentials_path, config.url.as_deref()) else {
        return AuthSource::None;
    };
    match load_mcp_credential(&credentials_path, &config.name, url) {
        Ok(Some(credential)) => AuthSource::Stored(Box::new(StoredLogin {
            config: config.clone(),
            credentials_path,
            credential: Mutex::new(credential),
            refreshing: Mutex::new(()),
        })),
        Ok(None) | Err(_) => AuthSource::None,
    }
}

/// `Authorization: Bearer $VARIABLE`. The variable is read now, when the
/// server is connected.
fn bearer_from_env(server: &str, variable: &str) -> Result<HeaderValue, String> {
    let problem =
        |what: &str| format!("environment variable {variable} for MCP server '{server}' {what}");
    let token = match std::env::var(variable) {
        Ok(token) if !token.is_empty() => token,
        Ok(_) => return Err(problem("is empty")),
        Err(std::env::VarError::NotPresent) => return Err(problem("is not set")),
        Err(std::env::VarError::NotUnicode(_)) => return Err(problem("is not valid Unicode")),
    };
    bearer_header(&token).ok_or_else(|| problem("does not hold a valid bearer token"))
}

/// `Bearer <token>`, marked sensitive so that it is never logged.
fn bearer_header(token: &str) -> Option<HeaderValue> {
    let mut header = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    header.set_sensitive(true);
    Some(header)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use orca_core::config::mcp_credentials::{
        McpCredential, delete_mcp_credential, load_mcp_credential, save_mcp_credential,
    };
    use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

    use super::*;
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer, TokenGate};
    use crate::oauth::unix_now;
    use crate::transport::{McpTransport, connect_with_credentials};

    const LOGIN_REQUIRED: &str = "MCP server requires login: run 'orca mcp login docs'";

    /// A login stored for the server, with `access_token` and the refresh
    /// token `rt-1`.
    fn stored_login(
        server: &OAuthTestServer,
        access_token: &str,
        expires_at: Option<u64>,
    ) -> McpCredential {
        McpCredential {
            server_url: server.mcp_url(),
            access_token: access_token.to_string(),
            refresh_token: Some("rt-1".to_string()),
            expires_at,
            token_endpoint: format!("{}/token", server.url()),
            client_id: "configured-client".to_string(),
            resource: server.mcp_url(),
            scope: None,
        }
    }

    /// A credentials file in `home` that holds `credential` for `docs`.
    fn credentials_holding(home: &Path, credential: &McpCredential) -> PathBuf {
        let path = home.join("mcp-credentials.json");
        save_mcp_credential(&path, "docs", credential).expect("store a login");
        path
    }

    fn connect(config: &McpServerConfig, credentials: Option<PathBuf>) -> Box<dyn McpTransport> {
        connect_with_credentials(config, credentials)
            .unwrap_or_else(|error| panic!("connect to the MCP server: {error}"))
    }

    #[test]
    fn an_expired_token_is_refreshed_once() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = credentials_holding(
            home.path(),
            &stored_login(&server, "at-expired", Some(unix_now() - 10)),
        );
        let transport = connect(&server.config("docs"), Some(credentials.clone()));

        transport
            .initialize()
            .expect("initialize with the refreshed token");
        transport.list_tools().expect("list tools");

        assert_eq!(
            server.trail(),
            ["POST /token", "POST /mcp", "POST /mcp", "POST /mcp"],
            "the token is refreshed once, before the first request"
        );
        let refresh = server.requests_to("/token")[0].form();
        assert_eq!(refresh["grant_type"], "refresh_token");
        assert_eq!(refresh["refresh_token"], "rt-1");
        assert_eq!(refresh["client_id"], "configured-client");
        assert_eq!(refresh["resource"], server.mcp_url());
        for request in server.requests_to("/mcp") {
            assert_eq!(request.header_values("authorization"), ["Bearer at-2"]);
        }
        let stored = load_mcp_credential(&credentials, "docs", &server.mcp_url())
            .expect("read the credentials")
            .expect("the login is still stored");
        assert_eq!(stored.access_token, "at-2");
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-2"));
        assert!(stored.expires_at.is_some_and(|at| at > unix_now() + 3000));
    }

    #[test]
    fn a_rejected_token_is_refreshed_and_the_request_sent_again() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let credentials =
            credentials_holding(home.path(), &stored_login(&server, "at-revoked", None));
        let transport = connect(&server.config("docs"), Some(credentials));

        transport
            .initialize()
            .expect("initialize after refreshing the token");

        assert_eq!(
            server.trail(),
            ["POST /mcp", "POST /token", "POST /mcp", "POST /mcp"]
        );
        let requests = server.requests_to("/mcp");
        assert_eq!(
            requests[0].header("authorization"),
            Some("Bearer at-revoked")
        );
        assert_eq!(requests[1].header("authorization"), Some("Bearer at-2"));
        assert_eq!(requests[1].json()["method"], "initialize");
    }

    #[test]
    fn a_token_turned_away_after_a_refresh_needs_a_login() {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            reject_every_token: true,
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials =
            credentials_holding(home.path(), &stored_login(&server, "at-revoked", None));
        let transport = connect(&server.config("docs"), Some(credentials));

        let error = transport
            .initialize()
            .expect_err("a server that turns every token away must fail");

        assert_eq!(error, LOGIN_REQUIRED);
        assert_eq!(
            server.trail(),
            ["POST /mcp", "POST /token", "POST /mcp"],
            "a request refreshes the token at most once"
        );
    }

    #[test]
    fn a_failed_refresh_reports_login_required() {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            refuse_refresh: true,
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let login = stored_login(&server, "at-expired", Some(unix_now() - 10));
        let credentials = credentials_holding(home.path(), &login);
        let transport = connect(&server.config("docs"), Some(credentials.clone()));

        let error = transport
            .initialize()
            .expect_err("a refused refresh must fail");

        assert!(is_auth_required(&error), "{error}");
        assert_eq!(error, LOGIN_REQUIRED);
        assert_eq!(
            server.trail(),
            ["POST /token"],
            "the expired token is not sent"
        );
        assert_eq!(
            load_mcp_credential(&credentials, "docs", &server.mcp_url()).expect("read"),
            Some(login),
            "a failed refresh leaves the stored login as it was"
        );
    }

    #[test]
    fn a_refresh_after_a_logout_does_not_store_the_login_again() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = credentials_holding(
            home.path(),
            &stored_login(&server, "at-expired", Some(unix_now() - 10)),
        );
        let transport = connect(&server.config("docs"), Some(credentials.clone()));
        // The user logs out while the server is connected.
        assert!(delete_mcp_credential(&credentials, "docs").expect("log out"));

        let error = transport
            .initialize()
            .expect_err("a login removed from the store must not be refreshed");

        assert_eq!(error, LOGIN_REQUIRED);
        assert!(server.requests().is_empty(), "{:?}", server.trail());
        assert_eq!(
            load_mcp_credential(&credentials, "docs", &server.mcp_url()).expect("read"),
            None
        );
    }

    #[test]
    fn a_logout_during_a_refresh_is_not_undone() {
        let gate = TokenGate::default();
        let server = OAuthTestServer::start(OAuthTestBehavior {
            hold_token_requests: Some(gate.clone()),
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = credentials_holding(
            home.path(),
            &stored_login(&server, "at-expired", Some(unix_now() - 10)),
        );
        let transport = connect(&server.config("docs"), Some(credentials.clone()));
        let initializing = std::thread::spawn(move || transport.initialize());
        assert!(
            server.wait_for_request("/token"),
            "the refresh never reached the token endpoint"
        );

        // The user logs out while the refresh is in flight.
        let logging_out = std::thread::spawn({
            let credentials = credentials.clone();
            move || delete_mcp_credential(&credentials, "docs")
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !logging_out.is_finished(),
            "the logout must wait for the refresh in flight"
        );
        gate.release();

        assert!(
            logging_out
                .join()
                .expect("the logout thread")
                .expect("log out"),
            "the logout removes the refreshed login"
        );
        initializing
            .join()
            .expect("the initializing thread")
            .expect("initialize with the refreshed token");
        assert_eq!(
            load_mcp_credential(&credentials, "docs", &server.mcp_url()).expect("read"),
            None,
            "the refresh must not undo the logout"
        );
    }

    #[test]
    fn a_token_another_orca_refreshed_is_used_as_it_is() {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            accepted_tokens: vec!["at-fresh".to_string()],
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = credentials_holding(
            home.path(),
            &stored_login(&server, "at-expired", Some(unix_now() - 10)),
        );
        let transport = connect(&server.config("docs"), Some(credentials.clone()));
        // Another Orca refreshes the login meanwhile, spending `rt-1`.
        save_mcp_credential(
            &credentials,
            "docs",
            &McpCredential {
                refresh_token: Some("rt-fresh".to_string()),
                ..stored_login(&server, "at-fresh", Some(unix_now() + 3600))
            },
        )
        .expect("store the other refresh");

        transport
            .initialize()
            .expect("initialize with the stored token");

        assert!(server.requests_to("/token").is_empty());
        for request in server.requests_to("/mcp") {
            assert_eq!(request.header_values("authorization"), ["Bearer at-fresh"]);
        }
    }

    #[test]
    fn a_server_without_a_stored_login_reports_login_required() {
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        let home = tempfile::tempdir().expect("temp dir");
        let transport = connect(
            &server.config("docs"),
            Some(home.path().join("mcp-credentials.json")),
        );

        let error = transport
            .initialize()
            .expect_err("a server that asks for a login must fail");

        assert_eq!(error, LOGIN_REQUIRED);
        assert_eq!(server.requests()[0].header("authorization"), None);
    }

    #[test]
    fn bearer_token_env_var_sets_the_authorization_header() {
        const VARIABLE: &str = "ORCA_TEST_MCP_BEARER_TOKEN_SETS_THE_HEADER";
        // SAFETY: set before this test starts any thread; nextest runs each
        // test in a process of its own.
        unsafe { std::env::set_var(VARIABLE, "env-token-1") };
        let server = OAuthTestServer::start(OAuthTestBehavior {
            accepted_tokens: vec!["env-token-1".to_string()],
            ..Default::default()
        });
        let mut config = server.config("docs");
        config.bearer_token_env_var = Some(VARIABLE.to_string());
        let transport = connect(&config, None);

        transport
            .initialize()
            .expect("initialize with the bearer token");
        transport.list_tools().expect("list tools");

        let requests = server.requests_to("/mcp");
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(
                request.header_values("authorization"),
                ["Bearer env-token-1"]
            );
        }
    }

    #[test]
    fn a_missing_bearer_env_var_is_reported() {
        const VARIABLE: &str = "ORCA_TEST_MCP_BEARER_TOKEN_IS_MISSING";
        // SAFETY: as above, before any thread starts.
        unsafe { std::env::remove_var(VARIABLE) };
        for transport in [McpTransportKind::Http, McpTransportKind::Sse] {
            let config = McpServerConfig {
                name: "docs".to_string(),
                transport,
                url: Some("http://127.0.0.1:9/mcp".to_string()),
                bearer_token_env_var: Some(VARIABLE.to_string()),
                ..Default::default()
            };
            let Err(error) = connect_with_credentials(&config, None) else {
                panic!("a missing variable must fail the connection");
            };
            assert_eq!(
                error,
                format!("environment variable {VARIABLE} for MCP server 'docs' is not set")
            );
        }
    }

    #[test]
    fn a_configured_authorization_header_is_sent_as_it_is() {
        const VARIABLE: &str = "ORCA_TEST_MCP_CONFIGURED_HEADER_WINS";
        // SAFETY: as above, before any thread starts.
        unsafe { std::env::set_var(VARIABLE, "env-token-1") };
        let server = OAuthTestServer::start(OAuthTestBehavior {
            accepted_tokens: vec!["configured-token".to_string()],
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials =
            credentials_holding(home.path(), &stored_login(&server, "at-stored", None));
        let mut config = server.config("docs");
        config.headers = HashMap::from([(
            "authorization".to_string(),
            "Bearer configured-token".to_string(),
        )]);
        config.bearer_token_env_var = Some(VARIABLE.to_string());
        let transport = connect(&config, Some(credentials));

        transport
            .initialize()
            .expect("initialize with the configured header");

        let requests = server.requests_to("/mcp");
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(
                request.header_values("authorization"),
                ["Bearer configured-token"]
            );
        }
        assert!(server.requests_to("/token").is_empty());
    }
}
