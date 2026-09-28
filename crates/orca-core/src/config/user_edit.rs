use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table};

use crate::config::file::USER_CONFIG_FILE;
use crate::mcp_types::{McpServerConfig, McpTransportKind};

/// Parse the user-owned config file under `dir`, hand the document to `edit`
/// for in-place mutation, and atomically write the result back, preserving
/// every untouched key, comment, and formatting choice. A missing file
/// starts from an empty document. An existing file that cannot be parsed is
/// rejected instead of overwritten, so a broken hand-written config is never
/// silently destroyed. When `edit` returns an error, nothing is written.
pub(crate) fn edit_user_config_in(
    dir: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let path = user_config_path_in(dir);
    std::fs::create_dir_all(dir)?;
    let mut document = read_document(&path)?;
    edit(&mut document)?;
    orca_platform::fs::atomic_write(
        &path,
        document.to_string().as_bytes(),
        orca_platform::fs::AtomicWritePolicy::NoFollow,
    )
    .map_err(|error| io::Error::other(format!("replacing {}: {error}", path.display())))?;
    Ok(path)
}

fn user_config_path_in(dir: &Path) -> PathBuf {
    dir.join(USER_CONFIG_FILE)
}

/// Read and parse the config file at `path`, starting from an empty
/// document when it does not exist. An existing file that cannot be parsed
/// is rejected with the same wording `persist_user_model_settings` uses, so
/// every caller in this module reports config corruption consistently.
fn read_document(path: &Path) -> io::Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.parse::<DocumentMut>().map_err(|error| {
            io::Error::other(format!(
                "{}: existing config cannot be parsed; fix or remove it before persisting settings ({error})",
                path.display()
            ))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(error) => Err(error),
    }
}

/// Validate an MCP server name: letters, digits, `-`, and `_`, without a
/// double underscore (`__`), which is reserved to separate the server and
/// tool segments of a tool's runtime name (`mcp__<server>__<tool>`).
pub fn validate_mcp_server_name(name: &str) -> Result<(), String> {
    let is_valid = !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
        && !name.contains("__");
    if is_valid {
        Ok(())
    } else {
        Err(format!(
            "invalid MCP server name '{name}': use letters, digits, '-' and '_', without '__'"
        ))
    }
}

/// Add `server` to the user-owned config's `[[mcp_servers]]` array.
pub fn add_user_mcp_server(server: &McpServerConfig) -> io::Result<PathBuf> {
    let dir = resolve_config_dir()?;
    add_user_mcp_server_in(&dir, server)
}

/// Add `server` to the `[[mcp_servers]]` array of the config file under
/// `dir`. Only non-empty/non-default fields are written; `env` and
/// `headers` are written as inline tables.
pub fn add_user_mcp_server_in(dir: &Path, server: &McpServerConfig) -> io::Result<PathBuf> {
    validate_mcp_server_name(&server.name).map_err(io::Error::other)?;
    let path = user_config_path_in(dir);
    edit_user_config_in(dir, |document| {
        let servers = mcp_servers_array_mut(document, &path)?;
        if servers
            .iter()
            .any(|table| table_name(table) == Some(server.name.as_str()))
        {
            return Err(io::Error::other(format!(
                "MCP server '{}' already exists in {}; remove it first with 'orca mcp remove {}'",
                server.name,
                path.display(),
                server.name
            )));
        }
        servers.push(server_to_table(server));
        Ok(())
    })
}

/// Remove every `[[mcp_servers]]` entry named `name` from the user-owned
/// config.
pub fn remove_user_mcp_server(name: &str) -> io::Result<PathBuf> {
    let dir = resolve_config_dir()?;
    remove_user_mcp_server_in(&dir, name)
}

/// Remove every `[[mcp_servers]]` entry named `name` from the config file
/// under `dir`.
pub fn remove_user_mcp_server_in(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = user_config_path_in(dir);
    edit_user_config_in(dir, |document| {
        let servers = mcp_servers_array_mut(document, &path)?;
        let before = servers.len();
        servers.retain(|table| table_name(table) != Some(name));
        if servers.len() == before {
            return Err(io::Error::other(format!(
                "no MCP server named '{name}' in {}",
                path.display()
            )));
        }
        Ok(())
    })
}

/// List every `[[mcp_servers]]` entry in the user-owned config.
pub fn list_user_mcp_servers() -> io::Result<(PathBuf, Vec<McpServerConfig>)> {
    let dir = resolve_config_dir()?;
    list_user_mcp_servers_in(&dir)
}

/// List every `[[mcp_servers]]` entry in the config file under `dir`. This
/// never writes: a missing file yields an empty list instead of creating
/// one.
pub fn list_user_mcp_servers_in(dir: &Path) -> io::Result<(PathBuf, Vec<McpServerConfig>)> {
    let path = user_config_path_in(dir);
    let document = read_document(&path)?;
    let servers = match document.get("mcp_servers") {
        None => Vec::new(),
        Some(item) => {
            let array = item
                .as_array_of_tables()
                .ok_or_else(|| not_an_array_error(&path))?;
            array
                .iter()
                .map(|table| {
                    toml::from_str::<McpServerConfig>(&table.to_string()).map_err(|error| {
                        io::Error::other(format!(
                            "{}: invalid MCP server entry: {error}",
                            path.display()
                        ))
                    })
                })
                .collect::<io::Result<Vec<_>>>()?
        }
    };
    Ok((path, servers))
}

fn resolve_config_dir() -> io::Result<PathBuf> {
    super::file::config_dir()
        .ok_or_else(|| io::Error::other("could not resolve the Orca configuration directory"))
}

/// Borrow the document's `mcp_servers` array of tables, creating an empty
/// one when the key is absent. Errors when the existing value is some other
/// TOML type instead of silently discarding it.
fn mcp_servers_array_mut<'a>(
    document: &'a mut DocumentMut,
    path: &Path,
) -> io::Result<&'a mut ArrayOfTables> {
    let item = document
        .entry("mcp_servers")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    item.as_array_of_tables_mut()
        .ok_or_else(|| not_an_array_error(path))
}

fn not_an_array_error(path: &Path) -> io::Error {
    io::Error::other(format!(
        "mcp_servers in {} is not an array of tables; edit it by hand",
        path.display()
    ))
}

fn table_name(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

fn server_to_table(server: &McpServerConfig) -> Table {
    let mut table = Table::new();
    table.insert("name", toml_edit::value(server.name.as_str()));
    table.insert(
        "transport",
        toml_edit::value(transport_str(&server.transport)),
    );
    if let Some(command) = &server.command {
        table.insert("command", toml_edit::value(command.as_str()));
    }
    if !server.args.is_empty() {
        table.insert(
            "args",
            toml_edit::value(server.args.iter().collect::<toml_edit::Array>()),
        );
    }
    if let Some(url) = &server.url {
        table.insert("url", toml_edit::value(url.as_str()));
    }
    if !server.env.is_empty() {
        table.insert("env", toml_edit::value(map_to_inline_table(&server.env)));
    }
    if !server.headers.is_empty() {
        table.insert(
            "headers",
            toml_edit::value(map_to_inline_table(&server.headers)),
        );
    }
    if let Some(value) = &server.bearer_token_env_var {
        table.insert("bearer_token_env_var", toml_edit::value(value.as_str()));
    }
    if let Some(value) = &server.oauth_client_id {
        table.insert("oauth_client_id", toml_edit::value(value.as_str()));
    }
    if let Some(value) = server.oauth_callback_port {
        table.insert("oauth_callback_port", toml_edit::value(i64::from(value)));
    }
    table
}

fn map_to_inline_table(map: &HashMap<String, String>) -> InlineTable {
    let mut inline = InlineTable::new();
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for key in keys {
        inline.insert(key.as_str(), map[key].as_str().into());
    }
    inline
}

fn transport_str(transport: &McpTransportKind) -> &'static str {
    match transport {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Sse => "sse",
        McpTransportKind::Http => "http",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_mcp_server_name_accepts_letters_digits_dash_and_underscore() {
        assert!(validate_mcp_server_name("docs").is_ok());
        assert!(validate_mcp_server_name("docs-2").is_ok());
        assert!(validate_mcp_server_name("docs_2").is_ok());
        assert!(validate_mcp_server_name("Docs9").is_ok());
    }

    #[test]
    fn validate_mcp_server_name_rejects_double_underscore_space_and_empty() {
        assert_eq!(
            validate_mcp_server_name("a__b"),
            Err(
                "invalid MCP server name 'a__b': use letters, digits, '-' and '_', without '__'"
                    .to_string()
            )
        );
        assert_eq!(
            validate_mcp_server_name("bad name"),
            Err(
                "invalid MCP server name 'bad name': use letters, digits, '-' and '_', without '__'"
                    .to_string()
            )
        );
        assert!(validate_mcp_server_name("").is_err());
    }
}
