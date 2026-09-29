use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
use orca_mcp::McpAuthKind;
use orca_mcp::oauth::OpenBrowser;

/// A request to add, list, show, remove, log in to, or log out of an MCP
/// server in the user config.
#[derive(Clone, Debug)]
pub enum McpCommandRequest {
    Add(McpAddRequest),
    List { json: bool },
    Get { name: String, json: bool },
    Remove { name: String },
    Login { name: String },
    Logout { name: String },
}

/// Raw, unvalidated fields for `orca mcp add`. String and list fields carry
/// CLI input verbatim; `run`/`run_in_with_browser` validate and parse them.
#[derive(Clone, Debug, Default)]
pub struct McpAddRequest {
    pub name: String,
    pub transport: Option<String>,
    pub env: Vec<String>,
    pub url: Option<String>,
    pub headers: Vec<String>,
    pub bearer_token_env_var: Option<String>,
    pub client_id: Option<String>,
    pub callback_port: Option<u16>,
    pub command: Vec<String>,
}

/// Run an `orca mcp` request against the resolved Orca configuration
/// directory, reading and writing real process stdio.
///
/// For `Login`, the browser opener prints the two lines the brief specifies
/// before opening the url: `orca_mcp::oauth::login` silently ignores an
/// opener error and keeps waiting for the callback, so the CLI itself is the
/// only place that ever prints the authorization url for the user to open by
/// hand. The closure prints straight to the process's own stdout rather than
/// through the `stdout` writer threaded through [`run_in_with_browser`]:
/// `OpenBrowser` is `Send` with no borrowed lifetime, so it cannot capture a
/// lock scoped to this function.
pub fn run(request: McpCommandRequest) -> i32 {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let Some(config_dir) = orca_core::config::file::config_dir() else {
        return fail(
            &mut stderr,
            "could not resolve the Orca configuration directory",
        );
    };
    let credentials_path =
        config_dir.join(orca_core::config::mcp_credentials::MCP_CREDENTIALS_FILE);
    let open_browser = login_browser_opener(&request);
    run_in_with_browser(
        &config_dir,
        &credentials_path,
        request,
        &mut stdout,
        &mut stderr,
        open_browser,
    )
}

/// The browser opener [`run`] passes to [`run_in_with_browser`]: for a
/// `Login` request it prints the brief's two lines before handing `url` to
/// `orca_platform::process::open_url`. Every other request never calls
/// `login`, so `open_browser` is never invoked for them and its body does
/// not matter.
fn login_browser_opener(request: &McpCommandRequest) -> OpenBrowser {
    let name = match request {
        McpCommandRequest::Login { name } => name.clone(),
        _ => String::new(),
    };
    Box::new(move |url: &str| {
        println!("Opening your browser to log in to MCP server {name}.");
        println!("If it does not open, visit: {url}");
        orca_platform::process::open_url(url)
    })
}

/// Run an `orca mcp` request against `config_dir` and `credentials_path`,
/// writing to the given streams and, for `Login`, opening the authorization
/// url with `open_browser`. Exposed separately from [`run`] so tests can
/// point it at a temporary directory, capture output, and drive the browser
/// step with a fixture instead of a real browser.
pub fn run_in_with_browser(
    config_dir: &Path,
    credentials_path: &Path,
    request: McpCommandRequest,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    open_browser: OpenBrowser,
) -> i32 {
    match request {
        McpCommandRequest::Add(request) => add(config_dir, request, stdout, stderr),
        McpCommandRequest::List { json } => {
            list(config_dir, credentials_path, json, stdout, stderr)
        }
        McpCommandRequest::Get { name, json } => {
            get(config_dir, credentials_path, &name, json, stdout, stderr)
        }
        McpCommandRequest::Remove { name } => {
            remove(config_dir, credentials_path, &name, stdout, stderr)
        }
        McpCommandRequest::Login { name } => login(
            config_dir,
            credentials_path,
            &name,
            stdout,
            stderr,
            open_browser,
        ),
        McpCommandRequest::Logout { name } => {
            logout(config_dir, credentials_path, &name, stdout, stderr)
        }
    }
}

fn add(
    config_dir: &Path,
    request: McpAddRequest,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let server = match build_server(request) {
        Ok(server) => server,
        Err(message) => return fail(stderr, &message),
    };
    match orca_core::config::user_edit::add_user_mcp_server_in(config_dir, &server) {
        Ok(path) => success(
            stdout,
            format_args!("added MCP server {} to {}", server.name, path.display()),
        ),
        Err(error) => fail(stderr, &error.to_string()),
    }
}

fn list(
    config_dir: &Path,
    credentials_path: &Path,
    json: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let (path, servers) = match orca_core::config::user_edit::list_user_mcp_servers_in(config_dir) {
        Ok(result) => result,
        Err(error) => return fail(stderr, &error.to_string()),
    };
    if json {
        let mut entries = Vec::with_capacity(servers.len());
        for server in &servers {
            match to_json(server, credentials_path) {
                Ok(entry) => entries.push(entry),
                Err(error) => return fail(stderr, &error.to_string()),
            }
        }
        return write_json(stdout, stderr, &entries);
    }
    if servers.is_empty() {
        return success(
            stdout,
            format_args!("no MCP servers configured in {}", path.display()),
        );
    }
    for server in &servers {
        let target = server_target(server);
        let transport = transport_str(&server.transport);
        let auth = match auth_status(server, credentials_path) {
            Ok(auth) => auth,
            Err(error) => return fail(stderr, &error.to_string()),
        };
        let line = if server.disabled {
            format!("{}\t{transport}\t{target}\tdisabled\t{auth}", server.name)
        } else {
            format!("{}\t{transport}\t{target}\t{auth}", server.name)
        };
        if writeln!(stdout, "{line}").is_err() {
            return 1;
        }
    }
    0
}

fn get(
    config_dir: &Path,
    credentials_path: &Path,
    name: &str,
    json: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let server = match find_configured_server(config_dir, name) {
        Ok(server) => server,
        Err(message) => return fail(stderr, &message),
    };
    if json {
        return match to_json(&server, credentials_path) {
            Ok(entry) => write_json(stdout, stderr, &entry),
            Err(error) => fail(stderr, &error.to_string()),
        };
    }
    let auth = match auth_status(&server, credentials_path) {
        Ok(auth) => auth,
        Err(error) => return fail(stderr, &error.to_string()),
    };
    print_server_text(stdout, &server, &auth)
}

fn remove(
    config_dir: &Path,
    credentials_path: &Path,
    name: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let path = match orca_core::config::user_edit::remove_user_mcp_server_in(config_dir, name) {
        Ok(path) => path,
        Err(error) => return fail(stderr, &error.to_string()),
    };
    // The config entry is already gone at this point: a failure below is
    // reported, but it cannot be undone by leaving the config unchanged.
    match orca_core::config::mcp_credentials::delete_mcp_credential(credentials_path, name) {
        Ok(_) => success(
            stdout,
            format_args!("removed MCP server {name} from {}", path.display()),
        ),
        Err(error) => fail(
            stderr,
            &format!(
                "removed MCP server '{name}' from {}, but failed to delete its saved login: {error}",
                path.display()
            ),
        ),
    }
}

fn login(
    config_dir: &Path,
    credentials_path: &Path,
    name: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    open_browser: OpenBrowser,
) -> i32 {
    let server = match find_configured_server(config_dir, name) {
        Ok(server) => server,
        Err(message) => return fail(stderr, &message),
    };
    if McpAuthKind::of(&server) != McpAuthKind::OAuth {
        return fail(
            stderr,
            &format!("MCP server '{name}' does not use OAuth login"),
        );
    }
    let options =
        orca_mcp::oauth::McpLoginOptions::new(credentials_path.to_path_buf(), open_browser);
    match orca_mcp::oauth::login(&server, options) {
        Ok(()) => success(stdout, format_args!("logged in to MCP server {name}")),
        Err(error) => fail(stderr, &error),
    }
}

fn logout(
    config_dir: &Path,
    credentials_path: &Path,
    name: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if let Err(message) = find_configured_server(config_dir, name) {
        return fail(stderr, &message);
    }
    match orca_core::config::mcp_credentials::delete_mcp_credential(credentials_path, name) {
        Ok(true) => success(stdout, format_args!("logged out of MCP server {name}")),
        Ok(false) => fail(stderr, &format!("MCP server '{name}' is not logged in")),
        Err(error) => fail(stderr, &error.to_string()),
    }
}

/// The configured server named `name`, or the "no MCP server named" error
/// that `get`, `remove`, `login`, and `logout` all report the same way for
/// one that does not exist.
fn find_configured_server(config_dir: &Path, name: &str) -> Result<McpServerConfig, String> {
    let (path, servers) = orca_core::config::user_edit::list_user_mcp_servers_in(config_dir)
        .map_err(|error| error.to_string())?;
    servers
        .into_iter()
        .find(|server| server.name == name)
        .ok_or_else(|| format!("no MCP server named '{name}' in {}", path.display()))
}

/// The auth status shown in `list`'s trailing column, `get`'s `auth:` line,
/// and the `--json` `auth` field. Computed from configuration and the
/// credentials file alone — this never connects to the server.
fn auth_status(server: &McpServerConfig, credentials_path: &Path) -> io::Result<String> {
    Ok(match McpAuthKind::of(server) {
        McpAuthKind::Stdio => "-".to_string(),
        McpAuthKind::StaticHeader => "static header".to_string(),
        McpAuthKind::BearerEnvVar(variable) => format!("bearer: ${variable}"),
        McpAuthKind::OAuth => {
            let url = server.url.as_deref().unwrap_or_default();
            let logged_in = orca_core::config::mcp_credentials::load_mcp_credential(
                credentials_path,
                &server.name,
                url,
            )?
            .is_some();
            if logged_in {
                "oauth: logged in".to_string()
            } else {
                "oauth: not logged in".to_string()
            }
        }
    })
}

/// Validate and translate raw `orca mcp add` input into a server
/// configuration. A non-empty `command` always means a stdio server; a bare
/// `url` is remote, with `transport` selecting `http` (the default) or
/// `sse`.
///
/// clap's derived `ArgGroup` enforces that exactly one of `--url` or a
/// trailing command is given (see `McpAddArgs` in `src/cli.rs`), but clap's
/// `requires` relation does not fire for an argument that is itself an
/// `ArgGroup` member, so `--header`, `--bearer-token-env-var`, `--client-id`,
/// and `--callback-port` cannot lean on `requires = "url"` there. This
/// function is the actual enforcement point for "these need --url".
fn build_server(request: McpAddRequest) -> Result<McpServerConfig, String> {
    if request.command.is_empty() && request.url.is_none() {
        return Err("either a command or --url is required".to_string());
    }

    let env = parse_key_value_pairs(&request.env, '=', "--env", "KEY=VALUE")?;

    if let Some((command, args)) = request.command.split_first() {
        require_absent(&request.headers, "--header")?;
        require_absent(&request.bearer_token_env_var, "--bearer-token-env-var")?;
        require_absent(&request.client_id, "--client-id")?;
        require_absent(&request.callback_port, "--callback-port")?;
        return Ok(McpServerConfig {
            name: request.name,
            transport: McpTransportKind::Stdio,
            command: Some(command.clone()),
            args: args.to_vec(),
            env,
            ..McpServerConfig::default()
        });
    }

    let transport = match request.transport.as_deref() {
        None | Some("http") => McpTransportKind::Http,
        Some("sse") => McpTransportKind::Sse,
        Some(value) => {
            return Err(format!(
                "invalid --transport value '{value}': expected 'http' or 'sse'"
            ));
        }
    };
    let headers = parse_key_value_pairs(&request.headers, ':', "--header", "'Name: value'")?
        .into_iter()
        .map(|(name, value)| (name, value.trim().to_string()))
        .collect();

    Ok(McpServerConfig {
        name: request.name,
        transport,
        url: request.url,
        env,
        headers,
        bearer_token_env_var: request.bearer_token_env_var,
        oauth_client_id: request.client_id,
        oauth_callback_port: request.callback_port,
        ..McpServerConfig::default()
    })
}

/// Parse `KEY=VALUE`-shaped entries (env: `=`, header: `:`), reporting the
/// first malformed one with `flag`'s name and `expected`'s format hint.
fn parse_key_value_pairs(
    entries: &[String],
    separator: char,
    flag: &str,
    expected: &str,
) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for entry in entries {
        match entry.split_once(separator) {
            Some((key, value)) => {
                map.insert(key.to_string(), value.to_string());
            }
            None => {
                return Err(format!(
                    "invalid {flag} value '{entry}': expected {expected}"
                ));
            }
        }
    }
    Ok(map)
}

/// Reject a `--url`-only option carried by a stdio (`command`-based) add
/// request. clap cannot enforce this itself (see `build_server`'s doc
/// comment), so this is the real check.
fn require_absent(value: &impl IsAbsent, flag: &str) -> Result<(), String> {
    if value.is_absent() {
        Ok(())
    } else {
        Err(format!("{flag} requires --url"))
    }
}

/// Whether a `McpAddRequest` field carrying optional CLI input was left
/// unset, used by [`require_absent`] across `Vec`, `Option<String>`, and
/// `Option<u16>` fields alike.
trait IsAbsent {
    fn is_absent(&self) -> bool;
}

impl IsAbsent for Vec<String> {
    fn is_absent(&self) -> bool {
        self.is_empty()
    }
}

impl<T> IsAbsent for Option<T> {
    fn is_absent(&self) -> bool {
        self.is_none()
    }
}

/// The `list` command's target column: a stdio server's command and its
/// arguments, joined with single spaces and no quoting, or a remote
/// server's url.
fn server_target(server: &McpServerConfig) -> String {
    match &server.command {
        Some(command) => {
            let mut parts = vec![command.clone()];
            parts.extend(server.args.iter().cloned());
            parts.join(" ")
        }
        None => server.url.clone().unwrap_or_default(),
    }
}

fn print_server_text(stdout: &mut impl Write, server: &McpServerConfig, auth: &str) -> i32 {
    let lines = [
        format!("name: {}", server.name),
        format!("transport: {}", transport_str(&server.transport)),
        format!("command: {}", server.command.as_deref().unwrap_or("-")),
        format!("args: {}", join_or_dash(&server.args)),
        format!("url: {}", server.url.as_deref().unwrap_or("-")),
        format!("env: {}", keys_or_dash(&server.env)),
        format!("headers: {}", keys_or_dash(&server.headers)),
        format!(
            "bearer_token_env_var: {}",
            server.bearer_token_env_var.as_deref().unwrap_or("-")
        ),
        format!(
            "oauth_client_id: {}",
            server.oauth_client_id.as_deref().unwrap_or("-")
        ),
        format!(
            "oauth_callback_port: {}",
            opt_to_string(server.oauth_callback_port)
        ),
        format!(
            "startup_timeout_ms: {}",
            opt_to_string(server.startup_timeout_ms)
        ),
        format!("tool_timeout_ms: {}", opt_to_string(server.tool_timeout_ms)),
        format!(
            "enabled_tools: {}",
            list_or_dash(server.enabled_tools.as_deref())
        ),
        format!(
            "disabled_tools: {}",
            list_or_dash(server.disabled_tools.as_deref())
        ),
        format!("disabled: {}", server.disabled),
        format!("auth: {auth}"),
    ];
    for line in lines {
        if writeln!(stdout, "{line}").is_err() {
            return 1;
        }
    }
    0
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(" ")
    }
}

fn list_or_dash(values: Option<&[String]>) -> String {
    match values {
        Some(values) => values.join(","),
        None => "-".to_string(),
    }
}

fn keys_or_dash(map: &HashMap<String, String>) -> String {
    if map.is_empty() {
        "-".to_string()
    } else {
        sorted_keys(map).join(",")
    }
}

fn opt_to_string<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn sorted_keys(map: &HashMap<String, String>) -> Vec<String> {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

fn non_empty(values: Vec<String>) -> Option<Vec<String>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

/// The `--json` shape for one server: field names are snake_case, and every
/// value that would be redacted or "-" in text mode is `null`. `auth` is
/// never redacted — it is a status word, never a secret.
#[derive(serde::Serialize)]
struct McpServerJson {
    name: String,
    transport: &'static str,
    command: Option<String>,
    args: Option<Vec<String>>,
    url: Option<String>,
    env_keys: Option<Vec<String>>,
    header_names: Option<Vec<String>>,
    bearer_token_env_var: Option<String>,
    oauth_client_id: Option<String>,
    oauth_callback_port: Option<u16>,
    startup_timeout_ms: Option<u64>,
    tool_timeout_ms: Option<u64>,
    enabled_tools: Option<Vec<String>>,
    disabled_tools: Option<Vec<String>>,
    disabled: bool,
    auth: String,
}

fn to_json(server: &McpServerConfig, credentials_path: &Path) -> io::Result<McpServerJson> {
    Ok(McpServerJson {
        name: server.name.clone(),
        transport: transport_str(&server.transport),
        command: server.command.clone(),
        args: non_empty(server.args.clone()),
        url: server.url.clone(),
        env_keys: non_empty(sorted_keys(&server.env)),
        header_names: non_empty(sorted_keys(&server.headers)),
        bearer_token_env_var: server.bearer_token_env_var.clone(),
        oauth_client_id: server.oauth_client_id.clone(),
        oauth_callback_port: server.oauth_callback_port,
        startup_timeout_ms: server.startup_timeout_ms,
        tool_timeout_ms: server.tool_timeout_ms,
        enabled_tools: server.enabled_tools.clone(),
        disabled_tools: server.disabled_tools.clone(),
        disabled: server.disabled,
        auth: auth_status(server, credentials_path)?,
    })
}

fn write_json(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    value: &impl serde::Serialize,
) -> i32 {
    match serde_json::to_string_pretty(value) {
        Ok(text) => success(stdout, format_args!("{text}")),
        Err(error) => fail(stderr, &error.to_string()),
    }
}

fn transport_str(transport: &McpTransportKind) -> &'static str {
    match transport {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Sse => "sse",
        McpTransportKind::Http => "http",
    }
}

fn success(writer: &mut impl Write, args: std::fmt::Arguments<'_>) -> i32 {
    if writeln!(writer, "{args}").is_ok() {
        0
    } else {
        1
    }
}

fn fail(writer: &mut impl Write, message: &str) -> i32 {
    let _ = writeln!(writer, "orca: {message}");
    1
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use orca_core::config::file::FileConfig;
    use orca_core::config::mcp_credentials::{
        McpCredential, load_mcp_credential, save_mcp_credential,
    };
    use orca_core::mcp_types::McpTransportKind;
    use orca_mcp::oauth::test_server::{OAuthTestBehavior, OAuthTestServer, test_browser};
    use tempfile::tempdir;

    use super::*;

    fn run(dir: &Path, request: McpCommandRequest) -> (i32, String, String) {
        run_with_browser(dir, request, Box::new(|_: &str| Ok(())))
    }

    fn run_with_browser(
        dir: &Path,
        request: McpCommandRequest,
        open_browser: OpenBrowser,
    ) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_in_with_browser(
            dir,
            &credentials_path(dir),
            request,
            &mut stdout,
            &mut stderr,
            open_browser,
        );
        (
            code,
            String::from_utf8(stdout).expect("stdout is UTF-8"),
            String::from_utf8(stderr).expect("stderr is UTF-8"),
        )
    }

    /// Where a test's credentials live, alongside its temporary config.toml.
    fn credentials_path(dir: &Path) -> std::path::PathBuf {
        dir.join("mcp-credentials.json")
    }

    fn add_request(name: &str, command: &[&str]) -> McpCommandRequest {
        McpCommandRequest::Add(McpAddRequest {
            name: name.to_string(),
            command: command.iter().map(|value| value.to_string()).collect(),
            ..McpAddRequest::default()
        })
    }

    fn config_path(dir: &Path) -> std::path::PathBuf {
        dir.join("config.toml")
    }

    fn read_config(dir: &Path) -> FileConfig {
        let content = fs::read_to_string(config_path(dir)).expect("config file exists");
        toml::from_str(&content).expect("config parses")
    }

    /// Adds `server` straight to the config file under `dir`, bypassing the
    /// CLI's own `add` translation layer — used by tests that need a server
    /// shaped a particular way (e.g. from an OAuth test fixture) rather than
    /// one `orca mcp add`'s raw string options can build.
    fn seed_server(dir: &Path, server: &McpServerConfig) {
        orca_core::config::user_edit::add_user_mcp_server_in(dir, server)
            .unwrap_or_else(|error| panic!("seed {}: {error}", server.name));
    }

    #[test]
    fn add_keeps_the_rest_of_the_config() {
        let temp = tempdir().unwrap();
        fs::write(
            config_path(temp.path()),
            "# keep me\nmodel = \"deepseek-flash\"\n",
        )
        .unwrap();

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                env: vec!["API_KEY=secret".to_string()],
                command: vec!["npx".to_string(), "-y".to_string(), "docs-mcp".to_string()],
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 0, "stderr: {stderr}");

        let content = fs::read_to_string(config_path(temp.path())).unwrap();
        assert!(content.contains("# keep me"), "{content}");
        assert!(content.contains("model = \"deepseek-flash\""), "{content}");

        let config = read_config(temp.path());
        assert_eq!(config.mcp_servers.len(), 1);
        let server = &config.mcp_servers[0];
        assert_eq!(server.name, "docs");
        assert_eq!(server.transport, McpTransportKind::Stdio);
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, vec!["-y".to_string(), "docs-mcp".to_string()]);
        assert_eq!(
            server.env.get("API_KEY").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn add_writes_a_remote_server() {
        let temp = tempdir().unwrap();

        let (code, stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "remote".to_string(),
                url: Some("https://example.com/mcp".to_string()),
                headers: vec!["Authorization: Bearer t".to_string()],
                bearer_token_env_var: Some("GH".to_string()),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        assert!(stdout.contains("added MCP server remote"), "{stdout}");

        let config = read_config(temp.path());
        let server = config
            .mcp_servers
            .iter()
            .find(|server| server.name == "remote")
            .expect("remote server exists");
        assert_eq!(server.transport, McpTransportKind::Http);
        assert_eq!(server.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(
            server.headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
        assert_eq!(server.bearer_token_env_var.as_deref(), Some("GH"));

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "remote-sse".to_string(),
                url: Some("https://example.com/sse".to_string()),
                transport: Some("sse".to_string()),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        let config = read_config(temp.path());
        let server = config
            .mcp_servers
            .iter()
            .find(|server| server.name == "remote-sse")
            .expect("remote-sse server exists");
        assert_eq!(server.transport, McpTransportKind::Sse);
    }

    #[test]
    fn add_rejects_a_duplicate_or_invalid_name() {
        let temp = tempdir().unwrap();
        fs::write(config_path(temp.path()), "model = \"deepseek-flash\"\n").unwrap();
        let baseline = fs::read_to_string(config_path(temp.path())).unwrap();

        let (code, _stdout, stderr) = run(temp.path(), add_request("a__b", &["true"]));
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: invalid MCP server name 'a__b': use letters, digits, '-' and '_', without '__'"
        );
        assert_eq!(
            fs::read_to_string(config_path(temp.path())).unwrap(),
            baseline
        );

        let (code, _stdout, stderr) = run(temp.path(), add_request("bad name", &["true"]));
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: invalid MCP server name 'bad name': use letters, digits, '-' and '_', without '__'"
        );
        assert_eq!(
            fs::read_to_string(config_path(temp.path())).unwrap(),
            baseline
        );

        let (code, _stdout, stderr) = run(temp.path(), add_request("docs", &["true"]));
        assert_eq!(code, 0, "stderr: {stderr}");
        let after_add = fs::read_to_string(config_path(temp.path())).unwrap();

        let (code, _stdout, stderr) = run(temp.path(), add_request("docs", &["true"]));
        assert_eq!(code, 1);
        let path = config_path(temp.path()).display().to_string();
        assert_eq!(
            stderr.trim_end(),
            format!(
                "orca: MCP server 'docs' already exists in {path}; remove it first with 'orca mcp remove docs'"
            )
        );
        assert_eq!(
            fs::read_to_string(config_path(temp.path())).unwrap(),
            after_add
        );
    }

    #[test]
    fn add_rejects_a_name_that_clashes_once_normalized() {
        let temp = tempdir().unwrap();
        let path = config_path(temp.path()).display().to_string();
        for (first, second, canonical) in [
            ("GitHub", "github", "github"),
            ("my-server", "my_server", "my_server"),
        ] {
            let (code, _stdout, stderr) = run(temp.path(), add_request(first, &["true"]));
            assert_eq!(code, 0, "stderr: {stderr}");
            let before = fs::read_to_string(config_path(temp.path())).unwrap();

            let (code, _stdout, stderr) = run(temp.path(), add_request(second, &["true"]));

            assert_eq!(code, 1);
            assert_eq!(
                stderr.trim_end(),
                format!(
                    "orca: MCP server name '{second}' clashes with '{first}' in {path}: Orca names both servers' tools mcp__{canonical}__*; choose another name, or remove '{first}' first with 'orca mcp remove {first}'"
                )
            );
            assert_eq!(
                fs::read_to_string(config_path(temp.path())).unwrap(),
                before
            );
        }

        for name in ["_", "-", "-_-"] {
            let (code, _stdout, stderr) = run(temp.path(), add_request(name, &["true"]));
            assert_eq!(code, 1, "{name}");
            assert_eq!(
                stderr.trim_end(),
                format!("orca: invalid MCP server name '{name}': it needs a letter or a digit")
            );
        }
        let servers = read_config(temp.path()).mcp_servers;
        assert_eq!(
            servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["GitHub", "my-server"]
        );
    }

    #[test]
    fn list_and_get_hide_secret_values() {
        let temp = tempdir().unwrap();
        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                env: vec!["API_KEY=secret".to_string()],
                command: vec!["npx".to_string(), "-y".to_string(), "docs-mcp".to_string()],
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "remote".to_string(),
                url: Some("https://example.com/mcp".to_string()),
                headers: vec!["Authorization: Bearer t".to_string()],
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 0, "stderr: {stderr}");

        let (code, stdout, _stderr) = run(temp.path(), McpCommandRequest::List { json: false });
        assert_eq!(code, 0);
        assert!(
            stdout
                .lines()
                .any(|line| line == "docs\tstdio\tnpx -y docs-mcp\t-"),
            "{stdout}"
        );
        assert!(
            stdout
                .lines()
                .any(|line| line == "remote\thttp\thttps://example.com/mcp\tstatic header"),
            "{stdout}"
        );
        assert!(!stdout.contains("secret"), "{stdout}");
        assert!(!stdout.contains("Bearer t"), "{stdout}");

        let (code, stdout, _stderr) = run(temp.path(), McpCommandRequest::List { json: true });
        assert_eq!(code, 0);
        assert!(!stdout.contains("secret"), "{stdout}");
        assert!(!stdout.contains("Bearer t"), "{stdout}");

        let (code, stdout, _stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "docs".to_string(),
                json: false,
            },
        );
        assert_eq!(code, 0);
        assert!(
            stdout.lines().any(|line| line == "env: API_KEY"),
            "{stdout}"
        );
        assert!(stdout.lines().any(|line| line == "auth: -"), "{stdout}");
        assert!(!stdout.contains("secret"), "{stdout}");

        let (code, stdout, _stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "docs".to_string(),
                json: true,
            },
        );
        assert_eq!(code, 0);
        assert!(stdout.contains("\"auth\": \"-\""), "{stdout}");
        assert!(!stdout.contains("secret"), "{stdout}");

        let (code, stdout, _stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "remote".to_string(),
                json: false,
            },
        );
        assert_eq!(code, 0);
        assert!(
            stdout.lines().any(|line| line == "headers: Authorization"),
            "{stdout}"
        );
        assert!(
            stdout.lines().any(|line| line == "auth: static header"),
            "{stdout}"
        );
        assert!(!stdout.contains("Bearer t"), "{stdout}");

        let (code, stdout, _stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "remote".to_string(),
                json: true,
            },
        );
        assert_eq!(code, 0);
        assert!(!stdout.contains("Bearer t"), "{stdout}");
    }

    #[test]
    fn remove_deletes_the_server_and_reports_a_missing_one() {
        let temp = tempdir().unwrap();
        let (code, _stdout, stderr) = run(temp.path(), add_request("docs", &["true"]));
        assert_eq!(code, 0, "stderr: {stderr}");

        let (code, stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Remove {
                name: "docs".to_string(),
            },
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        let path = config_path(temp.path()).display().to_string();
        assert_eq!(
            stdout.trim_end(),
            format!("removed MCP server docs from {path}")
        );

        let config = read_config(temp.path());
        assert!(config.mcp_servers.is_empty());

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Remove {
                name: "docs".to_string(),
            },
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            format!("orca: no MCP server named 'docs' in {path}")
        );
    }

    #[test]
    fn an_unparsable_config_is_never_overwritten() {
        let temp = tempdir().unwrap();
        fs::write(config_path(temp.path()), "model = [").unwrap();

        let (code, _stdout, stderr) = run(temp.path(), add_request("docs", &["true"]));
        assert_eq!(code, 1);
        assert!(stderr.contains("cannot be parsed"), "{stderr}");
        assert_eq!(
            fs::read_to_string(config_path(temp.path())).unwrap(),
            "model = ["
        );
    }

    #[test]
    fn env_and_header_values_must_be_well_formed() {
        let temp = tempdir().unwrap();

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                env: vec!["API_KEY".to_string()],
                command: vec!["true".to_string()],
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: invalid --env value 'API_KEY': expected KEY=VALUE"
        );

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "remote".to_string(),
                url: Some("https://example.com/mcp".to_string()),
                headers: vec!["NoColonHere".to_string()],
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: invalid --header value 'NoColonHere': expected 'Name: value'"
        );
    }

    #[test]
    fn add_rejects_an_invalid_transport() {
        let temp = tempdir().unwrap();
        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "remote".to_string(),
                url: Some("https://example.com/mcp".to_string()),
                transport: Some("carrier-pigeon".to_string()),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: invalid --transport value 'carrier-pigeon': expected 'http' or 'sse'"
        );
    }

    #[test]
    fn add_requires_a_command_or_url() {
        let temp = tempdir().unwrap();
        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "nothing".to_string(),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: either a command or --url is required"
        );
    }

    #[test]
    fn stdio_add_rejects_the_remote_only_options() {
        let temp = tempdir().unwrap();

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                command: vec!["true".to_string()],
                headers: vec!["X: y".to_string()],
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(stderr.trim_end(), "orca: --header requires --url");

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                command: vec!["true".to_string()],
                bearer_token_env_var: Some("TOK".to_string()),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: --bearer-token-env-var requires --url"
        );

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                command: vec!["true".to_string()],
                client_id: Some("CID".to_string()),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(stderr.trim_end(), "orca: --client-id requires --url");

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Add(McpAddRequest {
                name: "docs".to_string(),
                command: vec!["true".to_string()],
                callback_port: Some(8080),
                ..McpAddRequest::default()
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(stderr.trim_end(), "orca: --callback-port requires --url");

        assert!(!config_path(temp.path()).exists());
    }

    #[test]
    fn list_reports_when_no_servers_are_configured() {
        let temp = tempdir().unwrap();
        let (code, stdout, stderr) = run(temp.path(), McpCommandRequest::List { json: false });
        assert_eq!(code, 0, "stderr: {stderr}");
        let path = config_path(temp.path()).display().to_string();
        assert_eq!(
            stdout.trim_end(),
            format!("no MCP servers configured in {path}")
        );
    }

    #[test]
    fn login_stores_a_credential_and_logout_removes_it() {
        let temp = tempdir().unwrap();
        let server = OAuthTestServer::start(OAuthTestBehavior::default());
        seed_server(temp.path(), &server.config("docs"));

        let (browser, _page) = test_browser();
        let (code, stdout, stderr) = run_with_browser(
            temp.path(),
            McpCommandRequest::Login {
                name: "docs".to_string(),
            },
            browser,
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        assert_eq!(stdout.trim_end(), "logged in to MCP server docs");

        let credential =
            load_mcp_credential(&credentials_path(temp.path()), "docs", &server.mcp_url())
                .expect("read the credentials")
                .expect("a stored login");
        assert_eq!(credential.access_token, "at-1");

        let (code, stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Logout {
                name: "docs".to_string(),
            },
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        assert_eq!(stdout.trim_end(), "logged out of MCP server docs");
        assert!(
            load_mcp_credential(&credentials_path(temp.path()), "docs", &server.mcp_url())
                .expect("read the credentials")
                .is_none(),
            "logout must delete the stored credential"
        );

        // Logging out again, with no token left to remove, is its own error.
        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Logout {
                name: "docs".to_string(),
            },
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            "orca: MCP server 'docs' is not logged in"
        );
    }

    #[test]
    fn login_refuses_servers_without_oauth() {
        let temp = tempdir().unwrap();
        let cases = [
            McpServerConfig {
                name: "stdio".to_string(),
                transport: McpTransportKind::Stdio,
                command: Some("true".to_string()),
                ..McpServerConfig::default()
            },
            McpServerConfig {
                name: "static-header".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://headered.example/mcp".to_string()),
                headers: HashMap::from([("Authorization".to_string(), "Bearer t".to_string())]),
                ..McpServerConfig::default()
            },
            McpServerConfig {
                name: "lower-case-header".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://headered.example/mcp".to_string()),
                headers: HashMap::from([("authorization".to_string(), "Bearer t".to_string())]),
                ..McpServerConfig::default()
            },
            McpServerConfig {
                name: "bearer-var".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://beared.example/mcp".to_string()),
                bearer_token_env_var: Some("TOK".to_string()),
                ..McpServerConfig::default()
            },
        ];
        for server in &cases {
            seed_server(temp.path(), server);
        }

        for server in &cases {
            let (code, _stdout, stderr) = run(
                temp.path(),
                McpCommandRequest::Login {
                    name: server.name.clone(),
                },
            );
            assert_eq!(code, 1, "{}", server.name);
            assert_eq!(
                stderr.trim_end(),
                format!(
                    "orca: MCP server '{}' does not use OAuth login",
                    server.name
                ),
                "{}",
                server.name
            );
        }
    }

    #[test]
    fn login_and_logout_of_an_unconfigured_server_match_remove() {
        let temp = tempdir().unwrap();
        let path = config_path(temp.path()).display().to_string();

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Login {
                name: "ghost".to_string(),
            },
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            format!("orca: no MCP server named 'ghost' in {path}")
        );

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Logout {
                name: "ghost".to_string(),
            },
        );
        assert_eq!(code, 1);
        assert_eq!(
            stderr.trim_end(),
            format!("orca: no MCP server named 'ghost' in {path}")
        );
    }

    fn credential(server_url: &str) -> McpCredential {
        McpCredential {
            server_url: server_url.to_string(),
            access_token: "super-secret-access-token".to_string(),
            refresh_token: Some("super-secret-refresh-token".to_string()),
            expires_at: None,
            token_endpoint: "https://auth.example/token".to_string(),
            client_id: "client-1".to_string(),
            resource: server_url.to_string(),
            scope: None,
        }
    }

    #[test]
    fn status_output_never_contains_tokens() {
        let temp = tempdir().unwrap();
        let header_secret = "header-bearer-secret";

        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "stdio".to_string(),
                transport: McpTransportKind::Stdio,
                command: Some("true".to_string()),
                ..McpServerConfig::default()
            },
        );
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "static-header".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://headered.example/mcp".to_string()),
                headers: HashMap::from([(
                    "authorization".to_string(),
                    format!("Bearer {header_secret}"),
                )]),
                ..McpServerConfig::default()
            },
        );
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "bearer-var".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://beared.example/mcp".to_string()),
                bearer_token_env_var: Some("BEARED_TOKEN".to_string()),
                ..McpServerConfig::default()
            },
        );
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://mcp.example.test/mcp".to_string()),
                ..McpServerConfig::default()
            },
        );
        let docs_credential = credential("https://mcp.example.test/mcp");
        save_mcp_credential(&credentials_path(temp.path()), "docs", &docs_credential)
            .expect("seed a credential");

        let (list_code, list_text, list_stderr) =
            run(temp.path(), McpCommandRequest::List { json: false });
        let (list_json_code, list_json, list_json_stderr) =
            run(temp.path(), McpCommandRequest::List { json: true });
        let (docs_code, docs_text, docs_stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "docs".to_string(),
                json: false,
            },
        );
        let (docs_json_code, docs_json, docs_json_stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "docs".to_string(),
                json: true,
            },
        );
        let (headered_code, headered_text, headered_stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "static-header".to_string(),
                json: false,
            },
        );

        for (code, stderr) in [
            (list_code, &list_stderr),
            (list_json_code, &list_json_stderr),
            (docs_code, &docs_stderr),
            (docs_json_code, &docs_json_stderr),
            (headered_code, &headered_stderr),
        ] {
            assert_eq!(code, 0, "stderr: {stderr}");
        }
        for text in [
            &list_text,
            &list_json,
            &docs_text,
            &docs_json,
            &headered_text,
        ] {
            assert!(!text.contains(&docs_credential.access_token), "{text}");
            assert!(
                !text.contains(docs_credential.refresh_token.as_deref().unwrap()),
                "{text}"
            );
            assert!(!text.contains(header_secret), "{text}");
        }

        assert!(
            list_text
                .lines()
                .any(|line| line == "stdio\tstdio\ttrue\t-"),
            "{list_text}"
        );
        assert!(
            list_text
                .lines()
                .any(|line| line
                    == "static-header\thttp\thttps://headered.example/mcp\tstatic header"),
            "{list_text}"
        );
        assert!(
            list_text.lines().any(|line| line
                == "bearer-var\thttp\thttps://beared.example/mcp\tbearer: $BEARED_TOKEN"),
            "{list_text}"
        );
        assert!(
            list_text
                .lines()
                .any(|line| line == "docs\thttp\thttps://mcp.example.test/mcp\toauth: logged in"),
            "{list_text}"
        );

        assert!(
            list_json.contains("\"auth\": \"oauth: logged in\""),
            "{list_json}"
        );
        assert!(
            list_json.contains("\"auth\": \"static header\""),
            "{list_json}"
        );
        assert!(
            list_json.contains("\"auth\": \"bearer: $BEARED_TOKEN\""),
            "{list_json}"
        );
        assert!(list_json.contains("\"auth\": \"-\""), "{list_json}");

        assert!(
            docs_text
                .lines()
                .any(|line| line == "auth: oauth: logged in"),
            "{docs_text}"
        );
        assert!(
            docs_json.contains("\"auth\": \"oauth: logged in\""),
            "{docs_json}"
        );
        assert!(
            headered_text
                .lines()
                .any(|line| line == "auth: static header"),
            "{headered_text}"
        );
        assert!(
            headered_text
                .lines()
                .any(|line| line == "headers: authorization"),
            "{headered_text}"
        );

        // A server that has never logged in shows the other oauth status.
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "never-logged-in".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://never.example/mcp".to_string()),
                ..McpServerConfig::default()
            },
        );
        let (code, stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Get {
                name: "never-logged-in".to_string(),
                json: false,
            },
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        assert!(
            stdout
                .lines()
                .any(|line| line == "auth: oauth: not logged in"),
            "{stdout}"
        );
    }

    #[test]
    fn remove_also_deletes_the_saved_token() {
        let temp = tempdir().unwrap();
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://mcp.example.test/mcp".to_string()),
                ..McpServerConfig::default()
            },
        );
        save_mcp_credential(
            &credentials_path(temp.path()),
            "docs",
            &credential("https://mcp.example.test/mcp"),
        )
        .expect("seed a credential");

        let (code, stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Remove {
                name: "docs".to_string(),
            },
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        let path = config_path(temp.path()).display().to_string();
        assert_eq!(
            stdout.trim_end(),
            format!("removed MCP server docs from {path}")
        );
        assert!(
            load_mcp_credential(
                &credentials_path(temp.path()),
                "docs",
                "https://mcp.example.test/mcp"
            )
            .expect("read the credentials")
            .is_none(),
            "remove must delete the saved token too"
        );

        // Removing a server with no saved token at all is not an error.
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "search".to_string(),
                command: Some("true".to_string()),
                ..McpServerConfig::default()
            },
        );
        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Remove {
                name: "search".to_string(),
            },
        );
        assert_eq!(code, 0, "stderr: {stderr}");
    }

    #[test]
    fn remove_reports_a_token_deletion_error_after_the_config_change() {
        let temp = tempdir().unwrap();
        seed_server(
            temp.path(),
            &McpServerConfig {
                name: "docs".to_string(),
                command: Some("true".to_string()),
                ..McpServerConfig::default()
            },
        );
        // A credentials file that cannot be parsed makes deletion fail, but
        // `remove` must already have removed the config entry by the time it
        // tries.
        fs::write(credentials_path(temp.path()), "not json").unwrap();

        let (code, _stdout, stderr) = run(
            temp.path(),
            McpCommandRequest::Remove {
                name: "docs".to_string(),
            },
        );
        assert_eq!(code, 1);
        assert!(
            stderr.contains("removed MCP server 'docs'")
                && stderr.contains("failed to delete its saved login"),
            "{stderr}"
        );

        let config = read_config(temp.path());
        assert!(
            config.mcp_servers.is_empty(),
            "the config change must not be undone by the token-deletion failure"
        );
    }
}
