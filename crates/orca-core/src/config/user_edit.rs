use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{
    Array, ArrayOfTables, DocumentMut, InlineTable, Item, RawString, Table, TableLike, Value,
};

use crate::approval_rules::{PermissionRules, canonical_rule_tool};
use crate::config::error_text::{data_error_text, syntax_error_text};
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
    match read_config_text(path)? {
        Some(content) => content.parse::<DocumentMut>().map_err(|error| {
            unparsable_config_error(
                path,
                &syntax_error_text(error.message(), error.span(), &content),
            )
        }),
        None => Ok(DocumentMut::new()),
    }
}

/// The text of the config file at `path`, or `None` when it does not exist.
fn read_config_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// `what` is where and what the syntax error is, as `syntax_error_text` says
/// it: the config is never quoted, since it can hold secrets.
fn unparsable_config_error(path: &Path, what: &str) -> io::Error {
    io::Error::other(format!(
        "{}: existing config cannot be parsed; fix or remove it before persisting settings ({what})",
        path.display()
    ))
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
        servers.push(server_to_table(server));
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
        if mcp_servers_mut(document, &path)?.remove_where(|table| table_name(table) == Some(name))
            == 0
        {
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

/// List every `mcp_servers` entry in the config file under `dir`. This never
/// writes: a missing file yields an empty list instead of creating one.
///
/// The runtime loads the file with serde, so each entry is read that way
/// too, as the `McpServerConfig` it loads as. However the entry is written,
/// as a table with sub-tables (`[mcp_servers.env]`) or dotted keys, or
/// inline, in `[[mcp_servers]]` tables or in an inline array, it reads back
/// the same.
pub fn list_user_mcp_servers_in(dir: &Path) -> io::Result<(PathBuf, Vec<McpServerConfig>)> {
    let path = user_config_path_in(dir);
    let Some(content) = read_config_text(&path)? else {
        return Ok((path, Vec::new()));
    };
    let mut config: toml::Table = toml::from_str(&content).map_err(|error| {
        unparsable_config_error(
            &path,
            &syntax_error_text(error.message(), error.span(), &content),
        )
    })?;
    let servers = match config.remove("mcp_servers") {
        None => Vec::new(),
        Some(toml::Value::Array(entries)) => entries
            .into_iter()
            .map(|entry| read_server_entry(&path, entry))
            .collect::<io::Result<Vec<_>>>()?,
        Some(_) => return Err(not_an_array_error(&path, "mcp_servers")),
    };
    Ok((path, servers))
}

/// One `mcp_servers` entry as the server it loads as. When it does not load,
/// the error says where and what was expected, and shows nothing the entry
/// holds: `orca mcp list` and `get` print it, and a value can be a secret.
fn read_server_entry(path: &Path, entry: toml::Value) -> io::Result<McpServerConfig> {
    let invalid = |reason: &str| {
        io::Error::other(format!(
            "{}: invalid MCP server entry: {reason}",
            path.display()
        ))
    };
    match entry {
        toml::Value::Table(_) => entry
            .try_into()
            .map_err(|error| invalid(&data_error_text(&error))),
        other => Err(invalid(&format!(
            "expected a table, found {}",
            other.type_str()
        ))),
    }
}

/// Add an allow rule for `tool` to the user-owned config's
/// `permissions.rules`. Once loaded (the next session, or now, for a caller
/// that also grants it locally), it lets the matching tool call run without
/// asking.
pub fn add_user_allow_rule(tool: &str) -> io::Result<bool> {
    let dir = resolve_config_dir()?;
    add_user_allow_rule_in(&dir, tool)
}

/// Add an allow rule for `tool` to the `permissions.rules` of the config
/// file under `dir`, in the form the file already writes them: `rules` as
/// `[[permissions.rules]]` tables, or as an inline array, in a `[permissions]`
/// table or in an inline or dotted one. A file without rules gets
/// `[[permissions.rules]]` tables. The rule is written with no `pattern`, so
/// it covers every call of the tool. Returns `false` without writing when an
/// equivalent allow rule already exists, written either way (see
/// `is_equivalent_allow_rule`).
pub fn add_user_allow_rule_in(dir: &Path, tool: &str) -> io::Result<bool> {
    let path = user_config_path_in(dir);
    let mut appended = false;
    edit_user_config_in(dir, |document| {
        let mut rules = permission_rules_mut(document, &path)?;
        if rules
            .tables()
            .into_iter()
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
fn is_equivalent_allow_rule(table: &dyn TableLike, tool: &str) -> bool {
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

/// The permission rules of the user-owned config: its `permissions` table,
/// read on its own, so a value of the wrong type elsewhere in the file does
/// not keep them from being read here (a syntax error does). A session
/// loads the whole file instead: one that cannot, for a syntax error or a
/// value of the wrong type anywhere in it, starts with the defaults, and
/// none of these rules. A session's approvals follow the strictest of the
/// rules that match a call, so an allow rule saved there does not decide a
/// call one of the others asks about or denies.
pub fn user_permission_rules() -> io::Result<PermissionRules> {
    let dir = resolve_config_dir()?;
    user_permission_rules_in(&dir)
}

/// The permission rules of the config file under `dir`: none when there is
/// no file, and an error, which says where the file is wrong but never what
/// it holds, when it cannot be read.
pub fn user_permission_rules_in(dir: &Path) -> io::Result<PermissionRules> {
    #[derive(serde::Deserialize)]
    struct WithPermissions {
        #[serde(default)]
        permissions: PermissionRules,
    }

    let path = user_config_path_in(dir);
    let Some(content) = read_config_text(&path)? else {
        return Ok(PermissionRules::default());
    };
    let unreadable = |what: String| {
        io::Error::other(format!(
            "{}: cannot read the config: {what}",
            path.display()
        ))
    };
    let table = content
        .parse::<toml::Table>()
        .map_err(|error| unreadable(syntax_error_text(error.message(), error.span(), &content)))?;
    toml::Value::Table(table)
        .try_into::<WithPermissions>()
        .map(|config| config.permissions)
        .map_err(|error| unreadable(data_error_text(&error)))
}

fn resolve_config_dir() -> io::Result<PathBuf> {
    super::file::config_dir()
        .ok_or_else(|| io::Error::other("could not resolve the Orca configuration directory"))
}

/// An array of tables in a document, in the form the file writes it: tables
/// (`[[mcp_servers]]`) or an inline array (`mcp_servers = [{ name = "a" }]`).
/// Orca loads both, so an edit handles both, and leaves the file in the form
/// it found.
enum TableArray<'a> {
    Tables(&'a mut ArrayOfTables),
    Inline(&'a mut Array),
}

impl TableArray<'_> {
    /// Every table in the array.
    fn tables(&self) -> Vec<&dyn TableLike> {
        match self {
            Self::Tables(tables) => tables.iter().map(|table| table as &dyn TableLike).collect(),
            Self::Inline(entries) => entries
                .iter()
                .filter_map(Value::as_inline_table)
                .map(|table| table as &dyn TableLike)
                .collect(),
        }
    }

    /// The names of the tables that have one.
    fn names(&self) -> Vec<&str> {
        self.tables().into_iter().filter_map(table_name).collect()
    }

    /// Append `table`, as a table, or as an inline table in an inline array.
    fn push(&mut self, table: Table) {
        match self {
            Self::Tables(tables) => tables.push(table),
            Self::Inline(entries) => push_inline_entry(entries, table.into_inline_table()),
        }
    }

    /// Remove every table `is_match` accepts, and return how many there were.
    fn remove_where(&mut self, is_match: impl Fn(&dyn TableLike) -> bool) -> usize {
        match self {
            Self::Tables(tables) => {
                let before = tables.len();
                tables.retain(|table| !is_match(table));
                before - tables.len()
            }
            Self::Inline(entries) => {
                let mut removed = 0;
                loop {
                    let found = entries.iter().position(|entry| {
                        entry.as_inline_table().is_some_and(|table| is_match(table))
                    });
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

/// Borrow the array of tables in `item`, in whichever form it is written.
/// Errors when the item is some other TOML type instead of silently
/// discarding it, or an inline array with something else in it. `field`
/// names the item in the error.
fn table_array_in<'a>(item: &'a mut Item, path: &Path, field: &str) -> io::Result<TableArray<'a>> {
    match item {
        Item::ArrayOfTables(tables) => Ok(TableArray::Tables(tables)),
        Item::Value(Value::Array(entries)) if entries.iter().all(Value::is_inline_table) => {
            Ok(TableArray::Inline(entries))
        }
        _ => Err(not_an_array_error(path, field)),
    }
}

/// Borrow the document's `mcp_servers`, creating an empty array of tables
/// when the key is absent.
fn mcp_servers_mut<'a>(document: &'a mut DocumentMut, path: &Path) -> io::Result<TableArray<'a>> {
    let servers = document
        .entry("mcp_servers")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    table_array_in(servers, path, "mcp_servers")
}

/// Borrow the document's `permissions.rules`, creating the `permissions`
/// table and/or the `rules` array when either is absent. A new `rules` is
/// written as the table around it is: a value in an inline or a dotted
/// table, tables under a `[permissions]` header. Errors when an existing
/// value along that path is some other TOML type instead of silently
/// discarding it.
fn permission_rules_mut<'a>(
    document: &'a mut DocumentMut,
    path: &Path,
) -> io::Result<TableArray<'a>> {
    let permissions = document
        .entry("permissions")
        .or_insert_with(|| Item::Table(Table::new()));
    let written_as_values =
        permissions.is_inline_table() || permissions.as_table().is_some_and(Table::is_dotted);
    let permissions = permissions
        .as_table_like_mut()
        .ok_or_else(|| not_an_array_error(path, "permissions"))?;
    let rules = permissions.entry("rules").or_insert_with(|| {
        if written_as_values {
            Item::Value(Value::Array(Array::new()))
        } else {
            Item::ArrayOfTables(ArrayOfTables::new())
        }
    });
    table_array_in(rules, path, "permissions.rules")
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

fn not_an_array_error(path: &Path, field: &str) -> io::Error {
    io::Error::other(format!(
        "{field} in {} is not an array of tables; edit it by hand",
        path.display()
    ))
}

fn table_name(table: &dyn TableLike) -> Option<&str> {
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
    use crate::approval_rules::PermissionRule;
    use crate::approval_types::Decision;

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
    fn the_user_permission_rules_are_read_in_every_form_the_config_takes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);
        assert_eq!(
            user_permission_rules_in(dir.path()).unwrap(),
            PermissionRules::default()
        );
        for rules in [
            "[[permissions.rules]]\ntool = \"bash\"\npattern = \"rm *\"\ndecision = \"deny\"\n",
            "[permissions]\nrules = [{ tool = \"bash\", pattern = \"rm *\", decision = \"deny\" }]\n",
            "permissions.rules = [{ tool = \"bash\", pattern = \"rm *\", decision = \"deny\" }]\n",
        ] {
            std::fs::write(&path, format!("model = \"auto\"\n{rules}")).unwrap();
            assert_eq!(
                user_permission_rules_in(dir.path()).unwrap().rules,
                [PermissionRule::new("bash", "rm *", Decision::Deny)],
                "{rules}"
            );
        }

        // One that cannot be read says where it is, not what it holds.
        std::fs::write(
            &path,
            "[[permissions.rules]]\ntool = \"bash\"\ndecision = \"token-SECRET\"\n",
        )
        .unwrap();
        let error = user_permission_rules_in(dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains(USER_CONFIG_FILE), "{error}");
        assert!(!error.contains("SECRET"), "{error}");
        std::fs::write(&path, "permissions = \"token-SECRET\n").unwrap();
        let error = user_permission_rules_in(dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("TOML syntax error at line 1"), "{error}");
        assert!(!error.contains("SECRET"), "{error}");
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

    /// Everything `orca mcp list` and `get` show is read the way the runtime
    /// loads the file, so a server reads back the same however its entry is
    /// written.
    #[test]
    fn mcp_servers_are_read_however_they_are_written() {
        const CAPABILITIES: [&str; 6] = [
            "read = true",
            "write = true",
            "metadata_write = false",
            "network = false",
            "shell = false",
            "agent = false",
        ];
        let table_head = concat!(
            "[[mcp_servers]]\n",
            "name = \"docs\"\n",
            "command = \"npx\"\n",
            "args = [\"-y\", \"docs-mcp\"]\n",
            "enabled_tools = [\"search\"]\n",
        );
        let forms = [
            // Sub-tables of the entry.
            format!(
                "{table_head}\n[mcp_servers.env]\nAPI_KEY = \"k\"\n\n[mcp_servers.headers]\nX-Team = \"a\"\n\n[mcp_servers.capabilities]\n{}\n",
                CAPABILITIES.join("\n")
            ),
            // Dotted keys.
            format!(
                "{table_head}env.API_KEY = \"k\"\nheaders.X-Team = \"a\"\n{}\n",
                CAPABILITIES
                    .map(|line| format!("capabilities.{line}"))
                    .join("\n")
            ),
            // Inline tables.
            format!(
                "{table_head}env = {{ API_KEY = \"k\" }}\nheaders = {{ X-Team = \"a\" }}\ncapabilities = {{ {} }}\n",
                CAPABILITIES.join(", ")
            ),
            // An inline array, with inline tables.
            format!(
                "mcp_servers = [{{ name = \"docs\", command = \"npx\", args = [\"-y\", \"docs-mcp\"], enabled_tools = [\"search\"], env = {{ API_KEY = \"k\" }}, headers = {{ X-Team = \"a\" }}, capabilities = {{ {} }} }}]\n",
                CAPABILITIES.join(", ")
            ),
            // An inline array, with dotted keys.
            format!(
                "mcp_servers = [{{ name = \"docs\", command = \"npx\", args = [\"-y\", \"docs-mcp\"], enabled_tools = [\"search\"], env.API_KEY = \"k\", headers.X-Team = \"a\", {} }}]\n",
                CAPABILITIES
                    .map(|line| format!("capabilities.{line}"))
                    .join(", ")
            ),
        ];
        for form in forms {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(USER_CONFIG_FILE), &form).unwrap();

            let (_, servers) = list_user_mcp_servers_in(dir.path()).unwrap();

            assert_eq!(servers.len(), 1, "{form}");
            let docs = &servers[0];
            assert_eq!(docs.name, "docs", "{form}");
            assert_eq!(docs.command.as_deref(), Some("npx"), "{form}");
            assert_eq!(docs.args, ["-y", "docs-mcp"], "{form}");
            assert_eq!(
                docs.enabled_tools,
                Some(vec!["search".to_string()]),
                "{form}"
            );
            assert_eq!(docs.env.len(), 1, "{form}");
            assert_eq!(docs.env["API_KEY"], "k", "{form}");
            assert_eq!(docs.headers.len(), 1, "{form}");
            assert_eq!(docs.headers["X-Team"], "a", "{form}");
            assert!(docs.capabilities.write, "{form}");
        }
    }

    #[test]
    fn listing_servers_never_writes_and_reports_a_config_that_cannot_be_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);

        // No file is no servers, and no file is made.
        let (listed_path, servers) = list_user_mcp_servers_in(dir.path()).unwrap();
        assert_eq!(listed_path, path);
        assert!(servers.is_empty());
        assert!(!path.exists());
        assert!(!user_config_lock_path_in(dir.path()).exists());

        std::fs::write(&path, "model = [\n").unwrap();
        let error = list_user_mcp_servers_in(dir.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("existing config cannot be parsed"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model = [\n");
    }

    #[test]
    fn an_mcp_server_entry_that_does_not_load_is_reported() {
        for (content, reason) in [
            ("[[mcp_servers]]\ncommand = \"x\"\n", "missing field `name`"),
            (
                "mcp_servers = [{ name = \"a\", args = \"-y\" }]\n",
                "invalid type in `args`, expected a sequence",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(&path, content).unwrap();

            let error = list_user_mcp_servers_in(dir.path()).unwrap_err();

            assert_eq!(
                error.to_string(),
                format!("{}: invalid MCP server entry: {reason}", path.display())
            );
        }
    }

    /// What `orca mcp list` and `get` say about an entry that does not load is
    /// where it is and what was expected, never what the config holds there:
    /// the text lands in logs and bug reports, and a value can be a secret.
    #[test]
    fn an_mcp_server_entry_that_does_not_load_is_reported_without_its_values() {
        // (the entry, how it is reported, what must not be shown)
        let cases = [
            (
                "env = \"TOKEN=abc-SECRET\"",
                "invalid type in `env`, expected a map",
                "abc-SECRET",
            ),
            (
                "headers = \"Authorization: abc-SECRET\"",
                "invalid type in `headers`, expected a map",
                "abc-SECRET",
            ),
            (
                "args = \"--token=abc-SECRET\"",
                "invalid type in `args`, expected a sequence",
                "abc-SECRET",
            ),
            (
                "args = [\"ok\", 12345678]",
                "invalid type in `args`, expected a string",
                "12345678",
            ),
            (
                "[mcp_servers.env]\nPIN = 12345678",
                "invalid type in `env.PIN`, expected a string",
                "12345678",
            ),
            (
                "[mcp_servers.headers]\nAuthorization = 12345678",
                "invalid type in `headers.Authorization`, expected a string",
                "12345678",
            ),
            (
                "disabled = \"abc-SECRET\"",
                "invalid type in `disabled`, expected a boolean",
                "abc-SECRET",
            ),
            (
                "capabilities = { read = \"abc-SECRET\", write = true, metadata_write = false, network = false, shell = false, agent = false }",
                "invalid type in `capabilities.read`, expected a boolean",
                "abc-SECRET",
            ),
            (
                "oauth_callback_port = 70000",
                "invalid value in `oauth_callback_port`, expected u16",
                "70000",
            ),
            (
                "startup_timeout_ms = -123456",
                "invalid value in `startup_timeout_ms`, expected u64",
                "123456",
            ),
            // The name of a variant the config does not have is the config's,
            // too: it is not shown, and the ones it could have been are.
            (
                "transport = \"abc-SECRET\"",
                "unknown variant in `transport`, expected one of `stdio`, `sse`, `http`",
                "abc-SECRET",
            ),
        ];
        for (entry, reported, value) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(
                &path,
                format!("[[mcp_servers]]\nname = \"a\"\ncommand = \"x\"\n{entry}\n"),
            )
            .unwrap();

            let error = list_user_mcp_servers_in(dir.path()).unwrap_err();

            assert_eq!(
                error.to_string(),
                format!("{}: invalid MCP server entry: {reported}", path.display()),
                "{entry}"
            );
            assert!(!error.to_string().contains(value), "{entry}: {error}");
        }
    }

    /// A config that cannot be parsed is reported by line and column, and its
    /// text is never quoted: the line the parser stopped at can be a secret.
    #[test]
    fn a_config_that_cannot_be_parsed_is_reported_by_line_and_never_quoted() {
        let content = concat!(
            "# a comment\n",
            "model = \"m\"\n",
            "\n",
            "[[mcp_servers]]\n",
            "name = \"a\"\n",
            "command = \"x\"\n",
            "env = { TOKEN = \"abc-SECRET }\n",
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);
        std::fs::write(&path, content).unwrap();

        // Reading it, and every way of editing it, say the same.
        let errors = [
            list_user_mcp_servers_in(dir.path()).unwrap_err(),
            add_user_mcp_server_in(dir.path(), &stdio_server("b", "y")).unwrap_err(),
            remove_user_mcp_server_in(dir.path(), "a").unwrap_err(),
            add_user_allow_rule_in(dir.path(), "bash").unwrap_err(),
        ];
        for error in errors {
            assert_eq!(
                error.to_string(),
                format!(
                    "{}: existing config cannot be parsed; fix or remove it before persisting settings (TOML syntax error at line 7, column 30: invalid basic string)",
                    path.display()
                )
            );
        }
        // And the file is as it was.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn removing_and_adding_an_mcp_server_keep_the_sub_tables_of_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_CONFIG_FILE);
        std::fs::write(
            &path,
            concat!(
                "[[mcp_servers]]\n",
                "name = \"a\"\n",
                "command = \"x\"\n",
                "\n",
                "[mcp_servers.env]\n",
                "A_KEY = \"1\"\n",
                "\n",
                "[[mcp_servers]]\n",
                "name = \"c\"\n",
                "command = \"z\"\n",
                "\n",
                "[mcp_servers.env]\n",
                "C_KEY = \"3\"\n",
            ),
        )
        .unwrap();

        // The sub-table goes with its entry.
        remove_user_mcp_server_in(dir.path(), "a").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("A_KEY"), "{content}");
        let (_, servers) = list_user_mcp_servers_in(dir.path()).unwrap();
        assert_eq!(servers.len(), 1, "{content}");
        assert_eq!(servers[0].env["C_KEY"], "3", "{content}");

        // A new entry follows the sub-tables of the last, and does not
        // take them.
        add_user_mcp_server_in(dir.path(), &stdio_server("b", "y")).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let (_, servers) = list_user_mcp_servers_in(dir.path()).unwrap();
        assert_eq!(listed_names(dir.path()), ["c", "b"], "{content}");
        assert_eq!(servers[0].env["C_KEY"], "3", "{content}");
        assert!(servers[1].env.is_empty(), "{content}");
    }

    /// The rule saved by `add_user_allow_rule_in`, written as a table.
    const SAVED_RULE: &str = "{ tool = \"mcp__github__create_issue\", decision = \"allow\" }";

    #[test]
    fn an_allow_rule_is_saved_into_an_inline_rules_array() {
        const BASH: &str = "{ tool = \"bash\", pattern = \"cargo *\", decision = \"allow\" }";
        // What the file holds, and what it holds once the rule is saved.
        let cases = [
            (
                format!("permissions = {{ rules = [{BASH}] }}\n"),
                format!("permissions = {{ rules = [{BASH}, {SAVED_RULE}] }}\n"),
            ),
            (
                format!("[permissions]\nrules = [{BASH}]\n"),
                format!("[permissions]\nrules = [{BASH}, {SAVED_RULE}]\n"),
            ),
            (
                format!("permissions.rules = [{BASH}]\n"),
                format!("permissions.rules = [{BASH}, {SAVED_RULE}]\n"),
            ),
            // One rule to a line.
            (
                format!("[permissions]\nrules = [\n  {BASH},\n]\n"),
                format!("[permissions]\nrules = [\n  {BASH},\n  {SAVED_RULE},\n]\n"),
            ),
            // No rule yet, in a table written inline or with dotted keys.
            (
                "permissions = {}\n".to_string(),
                format!("permissions = {{ rules = [{SAVED_RULE}] }}\n"),
            ),
            (
                "permissions.rules = []\n".to_string(),
                format!("permissions.rules = [{SAVED_RULE}]\n"),
            ),
        ];
        for (before, after) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(&path, &before).unwrap();

            assert!(
                add_user_allow_rule_in(dir.path(), "mcp__github__create_issue").unwrap(),
                "{before}"
            );

            let saved = std::fs::read_to_string(&path).unwrap();
            assert_eq!(saved, after);
            // Once saved, the rule is in the file the runtime loads.
            let config: crate::config::file::FileConfig = toml::from_str(&saved).unwrap();
            assert!(
                config
                    .permissions
                    .rules
                    .contains(&PermissionRule::whole_tool(
                        "mcp__github__create_issue",
                        Decision::Allow
                    )),
                "{saved}"
            );
            // And saving it again changes nothing.
            assert!(!add_user_allow_rule_in(dir.path(), "mcp__github__create_issue").unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        }
    }

    #[test]
    fn an_inline_rule_counts_as_an_allow_rule_already_saved() {
        // (the rule in the file, the tool to save, whether the file changes)
        let cases = [
            (
                "{ tool = \"mcp__docs__search\", pattern = \"*\", decision = \"allow\" }",
                "mcp__docs__search",
                false,
            ),
            (
                "{ tool = \"mcp__My-Docs__*\", decision = \"allow\" }",
                "mcp__my_docs__*",
                false,
            ),
            (
                "{ tool = \"write_file\", pattern = \"*\", decision = \"allow\" }",
                "write_file",
                true,
            ),
            (
                "{ tool = \"mcp__docs__search\", decision = \"deny\" }",
                "mcp__docs__search",
                true,
            ),
        ];
        for (rule, tool, saved) in cases {
            for layout in [
                format!("permissions = {{ rules = [{rule}] }}\n"),
                format!("[permissions]\nrules = [{rule}]\n"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join(USER_CONFIG_FILE), &layout).unwrap();

                assert_eq!(
                    add_user_allow_rule_in(dir.path(), tool).unwrap(),
                    saved,
                    "{layout}"
                );
            }
        }
    }

    #[test]
    fn an_allow_rule_is_not_saved_into_a_value_that_is_not_a_list_of_tables() {
        for content in [
            "permissions = \"allow\"\n",
            "[permissions]\nrules = \"bash\"\n",
            "permissions = { rules = [\"bash\"] }\n",
            "[permissions.rules]\ntool = \"bash\"\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(USER_CONFIG_FILE);
            std::fs::write(&path, content).unwrap();

            let error = add_user_allow_rule_in(dir.path(), "bash").unwrap_err();

            assert!(
                error.to_string().contains("is not an array of tables"),
                "{content}: {error}"
            );
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
