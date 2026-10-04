use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, RawString, Table, Value};

use crate::approval_rules::canonical_rule_tool;
use crate::config::file::USER_CONFIG_FILE;
use crate::mcp_types::{McpServerConfig, McpTransportKind, canonical_mcp_name};

/// Parse the user-owned config file under `dir`, hand the document to `edit`
/// for in-place mutation, and atomically write the result back, preserving
/// every untouched key, comment, and formatting choice. A missing file
/// starts from an empty document. An existing file that cannot be parsed is
/// rejected instead of overwritten, so a broken hand-written config is never
/// silently destroyed. When `edit` returns an error, nothing is written.
///
/// The edit holds an exclusive lock on `config.toml.lock`, beside the file,
/// from the read to the write. The file is replaced whole, so two edits that
/// overlap, in this process or in another (`orca mcp add` while the TUI saves
/// an "always allow" rule), would both start from the same text, and the
/// later write would erase the earlier change. The lock does not nest:
/// `edit` must not call another editing helper, which would wait for it
/// forever. Readers take no lock, since the file is only ever replaced
/// whole.
pub(crate) fn edit_user_config_in(
    dir: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let path = user_config_path_in(dir);
    std::fs::create_dir_all(dir)?;
    let lock_path = user_config_lock_path_in(dir);
    let _lock = orca_platform::fs::ExclusiveFileLock::acquire(&lock_path)
        .map_err(|error| io::Error::other(format!("locking {}: {error}", lock_path.display())))?;
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

/// The file whose lock serializes every edit of the config file under `dir`.
fn user_config_lock_path_in(dir: &Path) -> PathBuf {
    dir.join(format!("{USER_CONFIG_FILE}.lock"))
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
/// tool segments of a tool's runtime name (`mcp__<server>__<tool>`), and
/// with a letter or a digit, so the canonical name its tools are named with
/// is not empty.
pub fn validate_mcp_server_name(name: &str) -> Result<(), String> {
    let is_valid = !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
        && !name.contains("__");
    if !is_valid {
        return Err(format!(
            "invalid MCP server name '{name}': use letters, digits, '-' and '_', without '__'"
        ));
    }
    if canonical_mcp_name(name).is_empty() {
        return Err(format!(
            "invalid MCP server name '{name}': it needs a letter or a digit"
        ));
    }
    Ok(())
}

/// Add `server` to the user-owned config's `mcp_servers`.
pub fn add_user_mcp_server(server: &McpServerConfig) -> io::Result<PathBuf> {
    let dir = resolve_config_dir()?;
    add_user_mcp_server_in(&dir, server)
}

/// Add `server` to the `mcp_servers` of the config file under `dir`, in the
/// form the file already writes them: an inline array stays an inline
/// array, and a file without the key gets `[[mcp_servers]]` tables. Only
/// non-empty/non-default fields are written; `env` and `headers` are written
/// as inline tables. A server whose name is taken, or has the canonical form
/// of a name that is (`GitHub` and `github`, `my-server` and `my_server`), is
/// refused: Orca would give both servers' tools the same names and connect
/// only one of them.
pub fn add_user_mcp_server_in(dir: &Path, server: &McpServerConfig) -> io::Result<PathBuf> {
    validate_mcp_server_name(&server.name).map_err(io::Error::other)?;
    let path = user_config_path_in(dir);
    let canonical = canonical_mcp_name(&server.name);
    edit_user_config_in(dir, |document| {
        let mut servers = mcp_servers_mut(document, &path)?;
        let names = servers.names();
        if names.contains(&server.name.as_str()) {
            return Err(io::Error::other(format!(
                "MCP server '{}' already exists in {}; remove it first with 'orca mcp remove {}'",
                server.name,
                path.display(),
                server.name
            )));
        }
        if let Some(existing) = names
            .iter()
            .find(|existing| canonical_mcp_name(existing) == canonical)
        {
            return Err(io::Error::other(format!(
                "MCP server name '{}' clashes with '{existing}' in {}: Orca names both servers' tools mcp__{canonical}__*; choose another name, or remove '{existing}' first with 'orca mcp remove {existing}'",
                server.name,
                path.display(),
            )));
        }
        servers.push(server);
        Ok(())
    })
}

/// Remove every `mcp_servers` entry named `name` from the user-owned
/// config.
pub fn remove_user_mcp_server(name: &str) -> io::Result<PathBuf> {
    let dir = resolve_config_dir()?;
    remove_user_mcp_server_in(&dir, name)
}

/// Remove every `mcp_servers` entry named `name` from the config file under
/// `dir`, whichever form the file writes them in.
pub fn remove_user_mcp_server_in(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = user_config_path_in(dir);
    edit_user_config_in(dir, |document| {
        if mcp_servers_mut(document, &path)?.remove(name) == 0 {
            return Err(io::Error::other(format!(
                "no MCP server named '{name}' in {}",
                path.display()
            )));
        }
        Ok(())
    })
}

/// List every `mcp_servers` entry in the user-owned config.
pub fn list_user_mcp_servers() -> io::Result<(PathBuf, Vec<McpServerConfig>)> {
    let dir = resolve_config_dir()?;
    list_user_mcp_servers_in(&dir)
}

/// List every `mcp_servers` entry in the config file under `dir`, whichever
/// form the file writes them in. This never writes: a missing file yields an
/// empty list instead of creating one.
pub fn list_user_mcp_servers_in(dir: &Path) -> io::Result<(PathBuf, Vec<McpServerConfig>)> {
    let path = user_config_path_in(dir);
    let document = read_document(&path)?;
    let servers = match document.get("mcp_servers") {
        None => Vec::new(),
        Some(Item::ArrayOfTables(tables)) => tables
            .iter()
            .map(|table| parse_server_entry(&path, &table.to_string()))
            .collect::<io::Result<Vec<_>>>()?,
        Some(Item::Value(Value::Array(entries))) => entries
            .iter()
            .map(|entry| match entry {
                // An inline table prints as `{ … }`, which is not a document;
                // as a table, it prints as the `key = value` lines of one.
                Value::InlineTable(table) => {
                    parse_server_entry(&path, &table.clone().into_table().to_string())
                }
                other => Err(io::Error::other(format!(
                    "{}: invalid MCP server entry: expected a table, found {}",
                    path.display(),
                    other.type_name()
                ))),
            })
            .collect::<io::Result<Vec<_>>>()?,
        Some(_) => return Err(not_an_array_error(&path, "mcp_servers")),
    };
    Ok((path, servers))
}

/// Read one server from the text of its entry: the `key = value` lines of a
/// table.
fn parse_server_entry(path: &Path, entry: &str) -> io::Result<McpServerConfig> {
    toml::from_str(entry).map_err(|error| {
        io::Error::other(format!(
            "{}: invalid MCP server entry: {error}",
            path.display()
        ))
    })
}

/// Add an allow rule for `tool` to the user-owned config's
/// `[[permissions.rules]]` array. Once loaded (the next session, or now, for
/// a caller that also grants it locally), it lets the matching tool call run
/// without asking.
pub fn add_user_allow_rule(tool: &str) -> io::Result<bool> {
    let dir = resolve_config_dir()?;
    add_user_allow_rule_in(&dir, tool)
}

/// Add an allow rule for `tool` to the `[[permissions.rules]]` array of the
/// config file under `dir`. The rule is written with no `pattern`, so it
/// covers every call of the tool. Returns `false` without writing when an
/// equivalent allow rule already exists (see `is_equivalent_allow_rule`).
pub fn add_user_allow_rule_in(dir: &Path, tool: &str) -> io::Result<bool> {
    let path = user_config_path_in(dir);
    let mut appended = false;
    edit_user_config_in(dir, |document| {
        let rules = permission_rules_array_mut(document, &path)?;
        if rules
            .iter()
            .any(|table| is_equivalent_allow_rule(table, tool))
        {
            return Ok(());
        }
        let mut rule = Table::new();
        rule.insert("tool", toml_edit::value(tool));
        rule.insert("decision", toml_edit::value("allow"));
        rules.push(rule);
        appended = true;
        Ok(())
    })?;
    Ok(appended)
}

/// Whether `table` is already an allow rule for every call of `tool`: the
/// shape `add_user_allow_rule_in` writes, without a `pattern`, or, for an
/// MCP tool or server, a hand-written `pattern = "*"`. An MCP call's target
/// is its tool name, which has no `/`, so `*` covers it; for a tool whose
/// target is a path, `*` stays within one directory and covers less. MCP
/// names are compared in canonical form, as rules match them.
fn is_equivalent_allow_rule(table: &Table, tool: &str) -> bool {
    let covers_every_call = match table.get("pattern") {
        None => true,
        Some(pattern) => tool.starts_with("mcp__") && pattern.as_str() == Some("*"),
    };
    table
        .get("tool")
        .and_then(Item::as_str)
        .is_some_and(|written| canonical_rule_tool(written) == canonical_rule_tool(tool))
        && table.get("decision").and_then(Item::as_str) == Some("allow")
        && covers_every_call
}

fn resolve_config_dir() -> io::Result<PathBuf> {
    super::file::config_dir()
        .ok_or_else(|| io::Error::other("could not resolve the Orca configuration directory"))
}

/// The entries of a document's `mcp_servers`, in the form the file writes
/// them: an array of tables (`[[mcp_servers]]`) or an inline array
/// (`mcp_servers = [{ name = "a", command = "x" }]`). Orca loads both, so an
/// edit handles both, and leaves the file in the form it found.
enum McpServerEntries<'a> {
    Tables(&'a mut ArrayOfTables),
    Inline(&'a mut Array),
}

impl McpServerEntries<'_> {
    /// The names of the entries that have one.
    fn names(&self) -> Vec<&str> {
        match self {
            Self::Tables(tables) => tables.iter().filter_map(table_name).collect(),
            Self::Inline(entries) => entries.iter().filter_map(inline_entry_name).collect(),
        }
    }

    /// Append `server`, written as a table, or as an inline table in an
    /// inline array.
    fn push(&mut self, server: &McpServerConfig) {
        let table = server_to_table(server);
        match self {
            Self::Tables(tables) => tables.push(table),
            Self::Inline(entries) => push_inline_entry(entries, table.into_inline_table()),
        }
    }

    /// Remove every entry named `name`, and return how many there were.
    fn remove(&mut self, name: &str) -> usize {
        match self {
            Self::Tables(tables) => {
                let before = tables.len();
                tables.retain(|table| table_name(table) != Some(name));
                before - tables.len()
            }
            Self::Inline(entries) => {
                let mut removed = 0;
                loop {
                    let found = entries
                        .iter()
                        .position(|entry| inline_entry_name(entry) == Some(name));
                    let Some(index) = found else {
                        return removed;
                    };
                    remove_inline_entry(entries, index);
                    removed += 1;
                }
            }
        }
    }
}

/// Borrow the document's `mcp_servers` entries, creating an empty array of
/// tables when the key is absent. Errors when the existing value is some
/// other TOML type instead of silently discarding it.
fn mcp_servers_mut<'a>(
    document: &'a mut DocumentMut,
    path: &Path,
) -> io::Result<McpServerEntries<'a>> {
    match document
        .entry("mcp_servers")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
    {
        Item::ArrayOfTables(tables) => Ok(McpServerEntries::Tables(tables)),
        Item::Value(Value::Array(entries)) if entries.iter().all(Value::is_inline_table) => {
            Ok(McpServerEntries::Inline(entries))
        }
        _ => Err(not_an_array_error(path, "mcp_servers")),
    }
}

/// The text before `entry` and the text after it: the whitespace and
/// comments `toml_edit` keeps with an entry.
fn entry_layout(entry: &Value) -> (String, String) {
    (
        decor_text(entry.decor().prefix()),
        decor_text(entry.decor().suffix()),
    )
}

fn decor_text(text: Option<&RawString>) -> String {
    text.and_then(RawString::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Append `entry` to an inline array. On one line it follows a comma and a
/// space, as `Array::push` writes it. In an array with an entry to a line, it
/// takes a line of its own, indented like the entry before it, and, when the
/// array has no trailing comma, the whitespace before the `]`.
///
/// Only whitespace is ever moved. `toml_edit` keeps a comment with the text
/// that follows it, so one that trails an entry stays where it is, which can
/// be after the entry that was added.
fn push_inline_entry(entries: &mut Array, entry: InlineTable) {
    let mut entry = Value::from(entry);
    let (prefix, suffix) = entries.iter().last().map(entry_layout).unwrap_or_default();
    let Some((_, indent)) = prefix
        .rsplit_once('\n')
        .filter(|(_, indent)| indent.trim().is_empty())
    else {
        return entries.push(entry);
    };
    entry.decor_mut().set_prefix(format!("\n{indent}"));
    if suffix.trim().is_empty() {
        entry.decor_mut().set_suffix(suffix);
        let last = entries.len() - 1;
        if let Some(last) = entries.get_mut(last) {
            last.decor_mut().set_suffix("");
        }
    }
    entries.push_formatted(entry);
}

/// Remove the entry at `index` from an inline array. The entry that becomes
/// the first or the last keeps the layout the array had at its start or its
/// end, unless a comment is in the way.
fn remove_inline_entry(entries: &mut Array, index: usize) {
    let removed = entries.remove(index);
    let (prefix, suffix) = entry_layout(&removed);
    if index == 0 {
        if let Some(first) = entries.get_mut(0)
            && prefix.trim().is_empty()
            && entry_layout(first).0.trim().is_empty()
        {
            first.decor_mut().set_prefix(prefix);
        }
    } else if index == entries.len()
        && let Some(last) = entries.get_mut(index - 1)
        && entry_layout(last).1.is_empty()
        && suffix.trim().is_empty()
    {
        last.decor_mut().set_suffix(suffix);
    }
}

/// Borrow the document's `permissions.rules` array of tables, creating the
/// `permissions` table and/or the `rules` array when either is absent.
/// Errors when an existing value along that path is some other TOML type
/// instead of silently discarding it.
fn permission_rules_array_mut<'a>(
    document: &'a mut DocumentMut,
    path: &Path,
) -> io::Result<&'a mut ArrayOfTables> {
    let permissions = document
        .entry("permissions")
        .or_insert_with(|| Item::Table(Table::new()));
    let permissions = permissions
        .as_table_mut()
        .ok_or_else(|| not_an_array_error(path, "permissions"))?;
    let rules = permissions
        .entry("rules")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    rules
        .as_array_of_tables_mut()
        .ok_or_else(|| not_an_array_error(path, "permissions.rules"))
}

fn not_an_array_error(path: &Path, field: &str) -> io::Error {
    io::Error::other(format!(
        "{field} in {} is not an array of tables; edit it by hand",
        path.display()
    ))
}

fn table_name(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

fn inline_entry_name(entry: &Value) -> Option<&str> {
    entry.as_inline_table()?.get("name")?.as_str()
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

    #[test]
    fn validate_mcp_server_name_rejects_a_name_without_a_letter_or_digit() {
        for name in ["_", "-", "-_-"] {
            assert_eq!(
                validate_mcp_server_name(name),
                Err(format!(
                    "invalid MCP server name '{name}': it needs a letter or a digit"
                ))
            );
        }
    }

    #[test]
    fn saving_an_allow_rule_appends_once() {
        let dir = tempfile::tempdir().unwrap();

        assert!(add_user_allow_rule_in(dir.path(), "mcp__github__create_issue").unwrap());
        assert!(!add_user_allow_rule_in(dir.path(), "mcp__github__create_issue").unwrap());

        let path = dir.path().join(USER_CONFIG_FILE);
        let document: DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let rules = document["permissions"]["rules"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(rules.len(), 1, "{document}");

        // A hand-written equivalent rule (`pattern = "*"` instead of an
        // omitted pattern) also counts as already existing.
        std::fs::write(
            &path,
            concat!(
                "[[permissions.rules]]\n",
                "tool = \"mcp__docs__search\"\n",
                "pattern = \"*\"\n",
                "decision = \"allow\"\n",
            ),
        )
        .unwrap();
        assert!(!add_user_allow_rule_in(dir.path(), "mcp__docs__search").unwrap());

        // A server rule written with the config name is the same rule.
        std::fs::write(
            &path,
            concat!(
                "[[permissions.rules]]\n",
                "tool = \"mcp__My-Docs__*\"\n",
                "decision = \"allow\"\n",
            ),
        )
        .unwrap();
        assert!(!add_user_allow_rule_in(dir.path(), "mcp__my_docs__*").unwrap());

        // For a tool whose target is a path, `*` covers only one directory,
        // so it is not the rule the caller asked for.
        std::fs::write(
            &path,
            concat!(
                "[[permissions.rules]]\n",
                "tool = \"write_file\"\n",
                "pattern = \"*\"\n",
                "decision = \"allow\"\n",
            ),
        )
        .unwrap();
        assert!(add_user_allow_rule_in(dir.path(), "write_file").unwrap());
        let document: DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let rules = document["permissions"]["rules"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(rules.len(), 2, "{document}");
        assert!(rules.get(1).unwrap().get("pattern").is_none(), "{document}");
    }

    fn stdio_server(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Stdio,
            command: Some(command.to_string()),
            ..McpServerConfig::default()
        }
    }

    fn listed_names(dir: &Path) -> Vec<String> {
        let (_, servers) = list_user_mcp_servers_in(dir).unwrap();
        servers.into_iter().map(|server| server.name).collect()
    }

    /// Eight writers each add ten rules at once. An edit reads the file,
    /// changes it, and replaces it whole, so without the lock a writer's
    /// replace erases every rule another writer added after it read.
    #[test]
    fn concurrent_config_edits_keep_every_change() {
        const THREADS: usize = 8;
        const EDITS: usize = 10;
        let dir = tempfile::tempdir().unwrap();
        // Every writer starts together, so the first reads all see the same file.
        let start = std::sync::Barrier::new(THREADS);

        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let (dir, start) = (dir.path(), &start);
                scope.spawn(move || {
                    start.wait();
                    for edit in 0..EDITS {
                        let tool = format!("tool_{thread}_{edit}");
                        assert!(add_user_allow_rule_in(dir, &tool).unwrap(), "{tool}");
                    }
                });
            }
        });

        let content = std::fs::read_to_string(dir.path().join(USER_CONFIG_FILE)).unwrap();
        let document: DocumentMut = content.parse().unwrap();
        let rules = document["permissions"]["rules"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(rules.len(), THREADS * EDITS, "{document}");
    }

    /// While another holder has `config.toml.lock`, an edit does not read,
    /// change, or write anything; it goes ahead the moment the lock is free.
    #[test]
    fn a_config_edit_waits_for_the_config_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);
        let lock =
            orca_platform::fs::ExclusiveFileLock::acquire(&dir.path().join("config.toml.lock"))
                .unwrap();
        let (finished_sender, finished) = std::sync::mpsc::channel();

        let editor = std::thread::spawn({
            let dir = dir.path().to_path_buf();
            move || {
                let appended = add_user_allow_rule_in(&dir, "bash");
                let _ = finished_sender.send(());
                appended
            }
        });

        assert!(
            finished
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the edit finished while the config lock was held"
        );
        assert!(
            !path.exists(),
            "the edit wrote while the config lock was held"
        );
        drop(lock);
        finished
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the edit finishes once the config lock is free");
        assert!(editor.join().unwrap().unwrap());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("tool = \"bash\"")
        );
    }

    #[test]
    fn inline_mcp_servers_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);
        std::fs::write(
            &path,
            concat!(
                "# keep this comment\n",
                "model = \"deepseek-flash\"\n",
                "\n",
                "# the servers\n",
                "mcp_servers = [{ name = \"a\", command = \"x\" }] # inline\n",
                "\n",
                "# and this one\n",
                "theme = \"auto\"\n",
            ),
        )
        .unwrap();

        let (listed_path, servers) = list_user_mcp_servers_in(dir.path()).unwrap();
        assert_eq!(listed_path, path);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "a");
        assert_eq!(servers[0].command.as_deref(), Some("x"));

        // `add` keeps the form the file uses, and still refuses a name that
        // is taken or that clashes with one once normalized.
        add_user_mcp_server_in(dir.path(), &stdio_server("b", "y")).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("[[mcp_servers]]"), "{content}");
        let document: DocumentMut = content.parse().unwrap();
        assert_eq!(document["mcp_servers"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(listed_names(dir.path()), ["a", "b"]);
        let error = add_user_mcp_server_in(dir.path(), &stdio_server("a", "z")).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        let error = add_user_mcp_server_in(dir.path(), &stdio_server("B", "z")).unwrap_err();
        assert!(error.to_string().contains("clashes with 'b'"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);

        remove_user_mcp_server_in(dir.path(), "a").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("[[mcp_servers]]"), "{content}");
        assert!(!content.contains("name = \"a\""), "{content}");
        assert_eq!(listed_names(dir.path()), ["b"]);
        let error = remove_user_mcp_server_in(dir.path(), "a").unwrap_err();
        assert!(
            error.to_string().contains("no MCP server named 'a'"),
            "{error}"
        );

        for kept in [
            "# keep this comment\n",
            "model = \"deepseek-flash\"\n",
            "# the servers\n",
            "# inline\n",
            "# and this one\n",
            "theme = \"auto\"\n",
        ] {
            assert!(content.contains(kept), "lost {kept:?}: {content}");
        }
    }

    #[test]
    fn inline_mcp_servers_keep_every_field() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(USER_CONFIG_FILE),
            concat!(
                "mcp_servers = [\n",
                "  { name = \"docs\", command = \"npx\", args = [\"-y\", \"docs-mcp\"], env = { API_KEY = \"k\" }, startup_timeout_ms = 5000, disabled = true },\n",
                "  { name = \"remote\", transport = \"http\", url = \"https://mcp.example/mcp\", headers = { \"X-Team\" = \"a\" }, bearer_token_env_var = \"TOKEN\", enabled_tools = [\"search\"] },\n",
                "]\n",
            ),
        )
        .unwrap();

        let (_, servers) = list_user_mcp_servers_in(dir.path()).unwrap();

        assert_eq!(servers.len(), 2);
        let docs = &servers[0];
        assert_eq!(docs.command.as_deref(), Some("npx"));
        assert_eq!(docs.args, ["-y", "docs-mcp"]);
        assert_eq!(docs.env.get("API_KEY").map(String::as_str), Some("k"));
        assert_eq!(docs.startup_timeout_ms, Some(5000));
        assert!(docs.disabled);
        let remote = &servers[1];
        assert_eq!(remote.transport, McpTransportKind::Http);
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example/mcp"));
        assert_eq!(remote.headers.get("X-Team").map(String::as_str), Some("a"));
        assert_eq!(remote.bearer_token_env_var.as_deref(), Some("TOKEN"));
        assert_eq!(
            remote.enabled_tools.as_deref(),
            Some(&["search".to_string()][..])
        );
    }

    #[test]
    fn a_new_inline_entry_is_laid_out_like_the_array_it_joins() {
        const B: &str = r#"{ name = "b", transport = "stdio", command = "y" }"#;
        // The file before, and after `b` is added. Removing `b` again must
        // give back the file as it was.
        let cases = [
            (
                "mcp_servers = [{ name = \"a\", command = \"x\" }]\n",
                "mcp_servers = [{ name = \"a\", command = \"x\" }, <b>]\n",
            ),
            ("mcp_servers = []\n", "mcp_servers = [<b>]\n"),
            (
                "mcp_servers = [\n  { name = \"a\", command = \"x\" },\n]\n",
                "mcp_servers = [\n  { name = \"a\", command = \"x\" },\n  <b>,\n]\n",
            ),
            (
                "mcp_servers = [\n    { name = \"a\", command = \"x\" }\n]\n",
                "mcp_servers = [\n    { name = \"a\", command = \"x\" },\n    <b>\n]\n",
            ),
            // Comments inside the array stay.
            (
                "mcp_servers = [\n  # the servers\n  { name = \"a\", command = \"x\" },\n  # { name = \"old\", command = \"z\" },\n]\n",
                "mcp_servers = [\n  # the servers\n  { name = \"a\", command = \"x\" },\n  <b>,\n  # { name = \"old\", command = \"z\" },\n]\n",
            ),
        ];
        for (before, after) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(&path, before).unwrap();

            add_user_mcp_server_in(dir.path(), &stdio_server("b", "y")).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                after.replace("<b>", B)
            );

            remove_user_mcp_server_in(dir.path(), "b").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[test]
    fn removing_an_inline_entry_keeps_the_array_as_written() {
        // The file before, the name removed, and the file after.
        let cases = [
            (
                "mcp_servers = [{ name = \"a\", command = \"x\" }, { name = \"c\", command = \"z\" }]\n",
                "a",
                "mcp_servers = [{ name = \"c\", command = \"z\" }]\n",
            ),
            (
                "mcp_servers = [{ name = \"a\", command = \"x\" }, { name = \"c\", command = \"z\" }]\n",
                "c",
                "mcp_servers = [{ name = \"a\", command = \"x\" }]\n",
            ),
            (
                "mcp_servers = [\n  { name = \"a\", command = \"x\" },\n  { name = \"c\", command = \"z\" },\n]\n",
                "a",
                "mcp_servers = [\n  { name = \"c\", command = \"z\" },\n]\n",
            ),
            (
                "mcp_servers = [\n  { name = \"a\", command = \"x\" },\n  { name = \"c\", command = \"z\" }\n]\n",
                "c",
                "mcp_servers = [\n  { name = \"a\", command = \"x\" }\n]\n",
            ),
            (
                "mcp_servers = [\n  { name = \"a\", command = \"x\" },\n  { name = \"c\", command = \"z\" },\n  { name = \"d\", command = \"w\" },\n]\n",
                "c",
                "mcp_servers = [\n  { name = \"a\", command = \"x\" },\n  { name = \"d\", command = \"w\" },\n]\n",
            ),
            // Every entry with the name goes.
            (
                "mcp_servers = [{ name = \"a\", command = \"x\" }, { name = \"a\", command = \"z\" }]\n",
                "a",
                "mcp_servers = []\n",
            ),
        ];
        for (before, name, after) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(&path, before).unwrap();

            remove_user_mcp_server_in(dir.path(), name).unwrap();

            assert_eq!(std::fs::read_to_string(&path).unwrap(), after, "{before}");
        }
    }

    #[test]
    fn an_mcp_servers_value_that_is_not_a_list_of_tables_is_reported_and_kept() {
        for (content, reported) in [
            ("mcp_servers = \"docs\"\n", "is not an array of tables"),
            (
                "mcp_servers = [\"docs\"]\n",
                "expected a table, found string",
            ),
            (
                "[mcp_servers.docs]\ncommand = \"x\"\n",
                "is not an array of tables",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(&path, content).unwrap();

            let error = list_user_mcp_servers_in(dir.path()).unwrap_err();
            assert!(error.to_string().contains(reported), "{content}: {error}");
            let error = add_user_mcp_server_in(dir.path(), &stdio_server("b", "y")).unwrap_err();
            assert!(
                error.to_string().contains("is not an array of tables"),
                "{content}: {error}"
            );
            assert!(remove_user_mcp_server_in(dir.path(), "docs").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn saving_an_allow_rule_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            &path,
            concat!(
                "# keep this comment\n",
                "model = \"deepseek-flash\"\n\n",
                "[[permissions.rules]]\n",
                "tool = \"bash\"\n",
                "pattern = \"cargo *\"\n",
                "decision = \"allow\"\n",
            ),
        )
        .unwrap();

        assert!(add_user_allow_rule_in(dir.path(), "mcp__github__create_issue").unwrap());

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(
            updated.contains("# keep this comment"),
            "comment lost: {updated:?}"
        );
        assert!(
            updated.contains("model = \"deepseek-flash\""),
            "unrelated key lost: {updated:?}"
        );
        let document: DocumentMut = updated.parse().unwrap();
        let rules = document["permissions"]["rules"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(rules.len(), 2, "{updated}");
    }
}
