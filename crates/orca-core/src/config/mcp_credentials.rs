//! The OAuth credentials `orca mcp login` stores for remote MCP servers, in
//! `$ORCA_HOME/mcp-credentials.json`: a JSON object keyed by server name.
//! The file holds tokens, so only its owner may read it, and it is written
//! the way `auth.json` is.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::file::config_dir;

pub const MCP_CREDENTIALS_FILE: &str = "mcp-credentials.json";

/// What logging in to one remote MCP server left behind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpCredential {
    /// The url of the server the tokens are for. They are not used for
    /// another url, even under the same server name.
    pub server_url: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// When the access token expires, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Where the access token is refreshed.
    pub token_endpoint: String,
    pub client_id: String,
    /// The resource the tokens were issued for (RFC 8707).
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// `$ORCA_HOME/mcp-credentials.json`, if the Orca directory can be resolved.
pub fn mcp_credentials_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(MCP_CREDENTIALS_FILE))
}

/// The credential stored for `server`, when it was stored for `server_url`.
pub fn load_mcp_credential(
    path: &Path,
    server: &str,
    server_url: &str,
) -> io::Result<Option<McpCredential>> {
    Ok(read_credentials(path)?
        .remove(server)
        .filter(|credential| credential.server_url == server_url))
}

/// Stores `credential` for `server`, replacing any stored before, and keeps
/// every other server's.
pub fn save_mcp_credential(
    path: &Path,
    server: &str,
    credential: &McpCredential,
) -> io::Result<()> {
    let mut credentials = read_credentials(path)?;
    credentials.insert(server.to_string(), credential.clone());
    write_credentials(path, &credentials)
}

/// Deletes the credential stored for `server`. Returns whether there was one.
pub fn delete_mcp_credential(path: &Path, server: &str) -> io::Result<bool> {
    let mut credentials = read_credentials(path)?;
    if credentials.remove(server).is_none() {
        return Ok(false);
    }
    write_credentials(path, &credentials)?;
    Ok(true)
}

type Credentials = BTreeMap<String, McpCredential>;

/// Reads every stored credential; a missing file holds none. A file that
/// cannot be read is reported rather than overwritten, and never quoted,
/// since it holds tokens.
fn read_credentials(path: &Path) -> io::Result<Credentials> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Credentials::new()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("reading {}: {error}", path.display()),
            ));
        }
    };
    serde_json::from_str(&content).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a valid MCP credentials file (line {}, column {})",
                path.display(),
                error.line(),
                error.column()
            ),
        )
    })
}

/// Writes the credentials so that only their owner can read them.
fn write_credentials(path: &Path, credentials: &Credentials) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let content = serde_json::to_string_pretty(credentials).map_err(io::Error::other)?;
    orca_platform::fs::atomic_write_private(
        path,
        content.as_bytes(),
        orca_platform::fs::AtomicWritePolicy::NoFollow,
    )
    .map_err(|error| io::Error::other(format!("writing {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(server_url: &str) -> McpCredential {
        McpCredential {
            server_url: server_url.to_string(),
            access_token: "at-1".to_string(),
            refresh_token: Some("rt-1".to_string()),
            expires_at: Some(1_900_000_000),
            token_endpoint: "https://auth.example/token".to_string(),
            client_id: "client-1".to_string(),
            resource: server_url.to_string(),
            scope: Some("mcp.read mcp.write".to_string()),
        }
    }

    #[test]
    fn credentials_round_trip_and_delete() {
        let home = tempfile::tempdir().expect("temp dir");
        // The Orca directory does not exist yet.
        let path = home.path().join("orca").join(MCP_CREDENTIALS_FILE);
        let docs = credential("https://docs.example/mcp");
        let search = McpCredential {
            refresh_token: None,
            expires_at: None,
            scope: None,
            ..credential("https://search.example/mcp")
        };
        assert_eq!(
            load_mcp_credential(&path, "docs", "https://docs.example/mcp").expect("no file yet"),
            None
        );

        save_mcp_credential(&path, "docs", &docs).expect("save docs");
        save_mcp_credential(&path, "search", &search).expect("save search");

        assert_eq!(
            load_mcp_credential(&path, "docs", "https://docs.example/mcp").expect("load docs"),
            Some(docs)
        );
        assert_eq!(
            load_mcp_credential(&path, "search", "https://search.example/mcp")
                .expect("load search"),
            Some(search.clone())
        );
        let stored: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read the file"))
                .expect("the file is JSON");
        assert_eq!(
            stored["search"],
            serde_json::json!({
                "server_url": "https://search.example/mcp",
                "access_token": "at-1",
                "token_endpoint": "https://auth.example/token",
                "client_id": "client-1",
                "resource": "https://search.example/mcp"
            }),
            "a credential without refresh token, expiry, or scope leaves them out"
        );

        assert!(delete_mcp_credential(&path, "docs").expect("delete docs"));
        assert_eq!(
            load_mcp_credential(&path, "docs", "https://docs.example/mcp").expect("load docs"),
            None
        );
        assert_eq!(
            load_mcp_credential(&path, "search", "https://search.example/mcp")
                .expect("load search"),
            Some(search),
            "deleting one server's credential keeps the others"
        );
        assert!(!delete_mcp_credential(&path, "docs").expect("delete docs again"));
    }

    #[test]
    fn a_credential_for_another_url_is_ignored() {
        let home = tempfile::tempdir().expect("temp dir");
        let path = home.path().join(MCP_CREDENTIALS_FILE);
        let docs = credential("https://docs.example/mcp");
        save_mcp_credential(&path, "docs", &docs).expect("save docs");

        assert_eq!(
            load_mcp_credential(&path, "docs", "https://moved.example/mcp").expect("load"),
            None
        );
        assert_eq!(
            load_mcp_credential(&path, "docs", "https://docs.example/mcp").expect("load"),
            Some(docs)
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_are_written_privately() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().expect("temp dir");
        let path = home.path().join(MCP_CREDENTIALS_FILE);
        let mode = |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;

        save_mcp_credential(&path, "docs", &credential("https://docs.example/mcp"))
            .expect("save docs");
        assert_eq!(mode(&path), 0o600);

        // A file some other tool left readable by everyone is tightened.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        save_mcp_credential(&path, "search", &credential("https://search.example/mcp"))
            .expect("save search");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_malformed_credentials_file_is_reported_and_kept() {
        let home = tempfile::tempdir().expect("temp dir");
        let path = home.path().join(MCP_CREDENTIALS_FILE);
        let malformed = r#"{"docs": "at-secret"}"#;
        fs::write(&path, malformed).expect("write a malformed file");

        let error = save_mcp_credential(&path, "search", &credential("https://search.example/mcp"))
            .expect_err("a malformed file must not be overwritten");
        assert!(
            error
                .to_string()
                .contains("is not a valid MCP credentials file"),
            "{error}"
        );
        assert!(
            !error.to_string().contains("at-secret"),
            "the error must not quote the file: {error}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), malformed);
        assert!(load_mcp_credential(&path, "docs", "https://docs.example/mcp").is_err());
    }
}
