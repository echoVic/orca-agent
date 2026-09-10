//! File-defined agents. Only discovery reads files; execution uses an owned snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::config::folder_trust;
use crate::model;
use crate::subagent_types::SubagentType;

pub const MAX_AGENT_FILE_BYTES: usize = 128 * 1024;
const MAX_AGENT_BODY_BYTES: usize = 64 * 1024;
const MAX_INHERITANCE_DEPTH: usize = 16;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    #[serde(deserialize_with = "ascii_string")]
    name: String,
    #[serde(deserialize_with = "yaml_string")]
    description: String,
    #[serde(default, deserialize_with = "present_string")]
    extends: Option<String>,
    #[serde(default, deserialize_with = "present_tools")]
    tools: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present_string")]
    model: Option<String>,
}

fn ascii_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    match serde_yaml_ng::Value::deserialize(deserializer)? {
        serde_yaml_ng::Value::String(value) if value.is_ascii() => Ok(value),
        _ => Err(serde::de::Error::custom("expected an ASCII string")),
    }
}

fn yaml_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    match serde_yaml_ng::Value::deserialize(deserializer)? {
        serde_yaml_ng::Value::String(value) => Ok(value),
        _ => Err(serde::de::Error::custom("expected a string")),
    }
}

fn present_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    ascii_string(deserializer).map(Some)
}

fn present_tools<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    let values = Vec::<serde_yaml_ng::Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| match value {
            serde_yaml_ng::Value::String(value) if value.is_ascii() => Ok(value),
            _ => Err(serde::de::Error::custom("tools must be ASCII strings")),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

#[derive(Clone, Debug)]
struct AgentDefinition {
    header: Frontmatter,
    body: String,
    source: PathBuf,
}

/// Fully resolved, serializable identity. No file path is interpreted during execution.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EffectiveAgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
}

impl EffectiveAgentDefinition {
    /// Additional caller restrictions only remove tools. Empty means no tools.
    pub fn narrow_tools(&mut self, ceiling: &[String]) {
        self.allowed_tools.retain(|tool| ceiling.contains(tool));
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        validate_description(&self.description)?;
        validate_body(&self.system_prompt)?;
        let mut names = BTreeSet::new();
        for tool in &self.allowed_tools {
            if !valid_tool_name(tool) || !names.insert(tool) {
                return Err("invalid or duplicate tool in effective agent".to_string());
            }
        }
        if let Some(model) = &self.model {
            validate_model_identifier(model)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDefinitionDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct AgentCatalog {
    pub agents: BTreeMap<String, EffectiveAgentDefinition>,
    pub diagnostics: Vec<AgentDefinitionDiagnostic>,
}

impl AgentCatalog {
    pub fn discover(cwd: &Path, known_tools: &[String], known_models: &[String]) -> Self {
        Self::discover_in(
            cwd,
            folder_trust::config_dir().as_deref(),
            known_tools,
            known_models,
        )
    }

    /// Explicit home variant avoids process-global environment mutation in embedders/tests.
    pub fn discover_in(
        cwd: &Path,
        home: Option<&Path>,
        known_tools: &[String],
        known_models: &[String],
    ) -> Self {
        let mut catalog = Self::default();
        let mut definitions = BTreeMap::new();
        if let Some(home) = home {
            catalog.collect_layer(
                home,
                &["agents"],
                known_tools,
                known_models,
                &mut definitions,
            );
            if let Ok(cwd) = cwd.canonicalize() {
                // Match the existing project convention: nearest Git root, otherwise cwd.
                let root = cwd
                    .ancestors()
                    .find(|path| path.join(".git").exists())
                    .unwrap_or(&cwd);
                if folder_trust::is_trusted_with_config_dir(&cwd, home)
                    && folder_trust::is_trusted_with_config_dir(root, home)
                {
                    catalog.collect_layer(
                        root,
                        &[".orca", "agents"],
                        known_tools,
                        known_models,
                        &mut definitions,
                    );
                }
            }
        }
        for (name, definition) in &definitions {
            match resolve(name, &definitions, &mut Vec::new()) {
                Ok(effective) => {
                    catalog.agents.insert(name.clone(), effective);
                }
                Err(message) => catalog.diagnostics.push(AgentDefinitionDiagnostic {
                    path: definition.source.clone(),
                    message,
                }),
            }
        }
        catalog
    }

    fn collect_layer(
        &mut self,
        base: &Path,
        components: &[&str],
        known_tools: &[String],
        known_models: &[String],
        definitions: &mut BTreeMap<String, AgentDefinition>,
    ) {
        let mut directory = base.to_path_buf();
        for component in components {
            directory.push(component);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                _ => {
                    self.diagnostics.push(AgentDefinitionDiagnostic {
                        path: directory,
                        message: "agent directories must be real directories, not symlinks".into(),
                    });
                    return;
                }
            }
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.diagnostics.push(AgentDefinitionDiagnostic {
                    path: directory,
                    message: error.to_string(),
                });
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "md") => {
                    paths.push(entry.path());
                }
                Ok(_) => {}
                Err(error) => self.diagnostics.push(AgentDefinitionDiagnostic {
                    path: directory.clone(),
                    message: error.to_string(),
                }),
            }
        }
        paths.sort();
        let mut layer = BTreeMap::new();
        let mut duplicates = BTreeSet::new();
        for path in paths {
            let content = read_definition(&path);
            let result = content
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|content| parse_definition(content, &path, known_tools, known_models));
            match result {
                Ok(definition) => {
                    let name = definition.header.name.clone();
                    if layer.insert(name.clone(), definition).is_some() {
                        duplicates.insert(name.clone());
                        self.diagnostics.push(AgentDefinitionDiagnostic {
                            path,
                            message: format!(
                                "duplicate agent '{name}'; all copies in this layer are disabled"
                            ),
                        });
                    }
                }
                Err(message) => {
                    // Invalid overrides block both a parseable declared identity and
                    // the filename identity; never fall back to a broader user policy.
                    if let Some(name) = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .filter(|name| validate_name(name).is_ok())
                    {
                        duplicates.insert(name.to_string());
                    }
                    if let Ok(content) = content {
                        let normalized = content.replace("\r\n", "\n");
                        if let Ok((header, _)) = split_frontmatter(&normalized)
                            && let Ok(value) =
                                serde_yaml_ng::from_str::<serde_yaml_ng::Value>(header)
                            && let Some(name) = value.get("name").and_then(|value| value.as_str())
                            && validate_name(name).is_ok()
                        {
                            duplicates.insert(name.to_string());
                        }
                    }
                    self.diagnostics
                        .push(AgentDefinitionDiagnostic { path, message });
                }
            }
        }
        for name in duplicates {
            layer.remove(&name);
            // Ambiguous project names must not silently fall back to a user definition.
            definitions.remove(&name);
        }
        definitions.extend(layer);
    }
}

fn read_definition(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("agent definitions must be regular files, not symlinks".into());
    }
    let file =
        orca_platform::fs::open_nofollow_nonblocking(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("agent definitions must be regular files".into());
    }
    let mut content = String::new();
    file.take((MAX_AGENT_FILE_BYTES + 1) as u64)
        .read_to_string(&mut content)
        .map_err(|error| error.to_string())?;
    if content.len() > MAX_AGENT_FILE_BYTES {
        return Err("agent definition exceeds 128 KiB".into());
    }
    Ok(content)
}

fn parse_definition(
    content: &str,
    path: &Path,
    known_tools: &[String],
    known_models: &[String],
) -> Result<AgentDefinition, String> {
    if content.len() > MAX_AGENT_FILE_BYTES {
        return Err("agent definition must be at most 128 KiB".into());
    }
    let normalized = content.replace("\r\n", "\n");
    let (yaml, body) = split_frontmatter(&normalized)?;
    let mut header: Frontmatter =
        serde_yaml_ng::from_str(yaml).map_err(|error| format!("invalid agent YAML: {error}"))?;
    validate_name(&header.name)?;
    validate_description(header.description.trim())?;
    if let Some(parent) = &header.extends
        && !SubagentType::is_builtin_name(parent)
    {
        validate_name(parent)?;
    }
    if let Some(tools) = &mut header.tools {
        let mut seen = BTreeSet::new();
        for tool in tools {
            if !valid_tool_name(tool) || !known_tools.contains(tool) {
                return Err(format!("unknown or invalid tool '{tool}'"));
            }
            *tool = canonical_tool_name(tool).to_string();
            if !seen.insert(tool.clone()) {
                return Err(format!("duplicate tool '{tool}'"));
            }
        }
    }
    if let Some(model) = &header.model {
        validate_model_identifier(model)?;
        if !model::preset_models().contains(&model.as_str()) && !known_models.contains(model) {
            return Err(format!("unknown agent model '{model}'"));
        }
    }
    let body = body.trim().to_string();
    validate_body(&body)?;
    Ok(AgentDefinition {
        header,
        body,
        source: path.to_path_buf(),
    })
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), String> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or("agent definition must start with a YAML frontmatter delimiter")?;
    // Only delimiters are scanned as lines. All frontmatter syntax is parsed by YAML.
    let end = rest
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let current = *offset;
            *offset += line.len();
            Some((current, line))
        })
        .find(|(_, line)| line.trim_end_matches('\n') == "---")
        .ok_or("agent definition is missing its closing frontmatter delimiter")?;
    Ok((&rest[..end.0], &rest[end.0 + end.1.len()..]))
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name.starts_with(|ch: char| ch.is_ascii_lowercase())
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        || SubagentType::is_builtin_name(name)
    {
        return Err(
            "agent name must be a non-reserved ASCII identifier ([a-z][a-z0-9_-]{0,63})".into(),
        );
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<(), String> {
    if description.trim().is_empty()
        || description.len() > 1024
        || description.chars().any(char::is_control)
    {
        return Err(
            "agent description must be 1-1024 UTF-8 bytes without control characters".into(),
        );
    }
    Ok(())
}

fn validate_body(body: &str) -> Result<(), String> {
    if body.trim().is_empty()
        || body.len() > MAX_AGENT_BODY_BYTES
        || body
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return Err("agent body must be nonempty UTF-8 Markdown, at most 64 KiB, without control characters".into());
    }
    Ok(())
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn canonical_tool_name(name: &str) -> &str {
    match name {
        "list_files" => "glob",
        _ => name,
    }
}

fn validate_model_identifier(name: &str) -> Result<(), String> {
    if name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/' | b'.' | b':'))
    {
        return Err("agent model must be a valid ASCII model identifier".into());
    }
    model::validate_model(name)
}

fn resolve(
    name: &str,
    definitions: &BTreeMap<String, AgentDefinition>,
    visiting: &mut Vec<String>,
) -> Result<EffectiveAgentDefinition, String> {
    if visiting.iter().any(|item| item == name) || visiting.len() >= MAX_INHERITANCE_DEPTH {
        return Err(format!("cyclic or excessive agent inheritance at '{name}'"));
    }
    let definition = definitions
        .get(name)
        .ok_or_else(|| format!("unknown parent agent '{name}'"))?;
    visiting.push(name.to_string());
    let parent_name = definition.header.extends.as_deref().unwrap_or("general");
    let mut effective = if SubagentType::is_builtin_name(parent_name) {
        let parent = SubagentType::from_str(parent_name);
        EffectiveAgentDefinition {
            name: name.to_string(),
            description: String::new(),
            system_prompt: parent.system_prompt_suffix().trim().to_string(),
            allowed_tools: parent
                .allowed_tools()
                .into_iter()
                .map(|name| canonical_tool_name(name).to_string())
                .collect(),
            model: None,
        }
    } else {
        resolve(parent_name, definitions, visiting)?
    };
    visiting.pop();
    effective.name = name.to_string();
    effective.description = definition.header.description.trim().to_string();
    if !effective.system_prompt.is_empty() {
        effective.system_prompt.push_str("\n\n");
    }
    effective.system_prompt.push_str(&definition.body);
    if let Some(tools) = &definition.header.tools {
        effective.narrow_tools(tools);
    }
    if definition.header.model.is_some() {
        effective.model = definition.header.model.clone();
    }
    effective.allowed_tools.sort();
    effective.allowed_tools.dedup();
    effective.validate()?;
    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use folder_trust::TrustLevel;

    fn tools() -> Vec<String> {
        SubagentType::General
            .allowed_tools()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn write_agent(directory: &Path, filename: &str, name: &str, extra: &str, body: &str) {
        fs::create_dir_all(directory).unwrap();
        fs::write(
            directory.join(filename),
            format!("---\nname: {name}\ndescription: Review source safely\n{extra}---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn yaml_supports_quoted_colons_folded_scalars_lists_and_crlf() {
        let text = "---\nname: audit\ndescription: >-\n  Review: source\n  safely\ntools:\n  - read_file\n  - 'grep'\nmodel: deepseek-v4-pro\n---\nInspect changes.\n";
        let agent = parse_definition(
            &text.replace('\n', "\r\n"),
            Path::new("audit.md"),
            &tools(),
            &[],
        )
        .unwrap();
        assert_eq!(agent.header.description, "Review: source safely");
        assert_eq!(agent.header.tools.unwrap(), vec!["read_file", "grep"]);
        assert_eq!(agent.body, "Inspect changes.");
    }

    #[test]
    fn invalid_frontmatter_and_content_fail_closed() {
        let valid = "---\nname: audit\ndescription: Review source\n---\nInspect code.\n";
        let invalid = [
            valid.replace("name: audit", "name: general"),
            valid.replace("name: audit", "name: reviewer"),
            valid.replace("name: audit", "name: ../audit"),
            valid.replace("name: audit", "name: Audit"),
            valid.replace("name: audit", "name: 123"),
            valid.replace("name: audit", "name: audit\nname: second"),
            valid.replace("name: audit", "name: audit\npermissions: unrestricted"),
            valid.replace("name: audit", "name: audit\ntools: read_file"),
            valid.replace("name: audit", "name: audit\ntools: [read_file, read_file]"),
            valid.replace("name: audit", "name: audit\ntools: [not_a_tool]"),
            valid.replace("name: audit", "name: audit\ntools: [123]"),
            valid.replace("name: audit", "name: audit\ntools: null"),
            valid.replace("name: audit", "name: audit\nmodel: not-a-model"),
            valid.replace("name: audit", "name: audit\nmodel: null"),
            valid.replace("description: Review source", "description: ''"),
            valid.replace("description: Review source", "description: 42"),
            valid.replace("Review source", "Review\u{1b} source"),
            valid.replace("Inspect code.", ""),
            valid.replace("Inspect code.", "\u{7f}"),
            valid.replace("name: audit", "name: \u{e9}"),
            valid.replacen("---\n", "", 1),
            valid.replace("\n---\n", "\n"),
        ];
        for text in invalid {
            assert!(
                parse_definition(&text, Path::new("audit.md"), &tools(), &[]).is_err(),
                "{text:?}"
            );
        }
        assert!(
            parse_definition(
                &format!("{valid}{}", "x".repeat(MAX_AGENT_BODY_BYTES)),
                Path::new("audit.md"),
                &tools(),
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn trusted_project_overrides_user_and_nested_untrusted_folder_is_excluded() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        let nested = project.path().join("nested");
        fs::create_dir(&nested).unwrap();
        write_agent(
            &home.path().join("agents"),
            "user.md",
            "audit",
            "",
            "User policy.",
        );
        write_agent(
            &project.path().join(".orca/agents"),
            "project.md",
            "audit",
            "",
            "Project policy.",
        );
        let discover = || AgentCatalog::discover_in(&nested, Some(home.path()), &tools(), &[]);
        assert_eq!(discover().agents["audit"].system_prompt, "User policy.");
        folder_trust::set_trust_with_config_dir(project.path(), home.path(), TrustLevel::Trusted)
            .unwrap();
        assert_eq!(discover().agents["audit"].system_prompt, "Project policy.");
        folder_trust::set_trust_with_config_dir(&nested, home.path(), TrustLevel::Untrusted)
            .unwrap();
        assert_eq!(discover().agents["audit"].system_prompt, "User policy.");
    }

    #[test]
    fn duplicate_names_disable_all_copies_without_falling_back() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        folder_trust::set_trust_with_config_dir(project.path(), home.path(), TrustLevel::Trusted)
            .unwrap();
        write_agent(
            &home.path().join("agents"),
            "audit.md",
            "audit",
            "",
            "User policy.",
        );
        let directory = project.path().join(".orca/agents");
        write_agent(&directory, "z.md", "audit", "", "Second.");
        write_agent(&directory, "a.md", "audit", "", "First.");
        write_agent(&directory, "valid.md", "valid", "", "Valid.");
        let catalog = AgentCatalog::discover_in(project.path(), Some(home.path()), &tools(), &[]);
        assert!(!catalog.agents.contains_key("audit"));
        assert_eq!(catalog.agents.keys().collect::<Vec<_>>(), vec!["valid"]);
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|entry| entry.message.contains("duplicate"))
        );
    }

    #[test]
    fn inheritance_intersects_tools_and_preserves_parent_instructions() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let directory = home.path().join("agents");
        write_agent(
            &directory,
            "parent.md",
            "audit",
            "extends: code_reviewer\ntools: [read_file, grep, bash]\nmodel: deepseek-v4-pro\n",
            "Parent policy.",
        );
        write_agent(
            &directory,
            "child.md",
            "focused",
            "extends: audit\ntools: [read_file, write_file]\n",
            "Child policy.",
        );
        write_agent(
            &directory,
            "empty.md",
            "empty",
            "extends: focused\ntools: []\n",
            "No tools.",
        );
        let catalog = AgentCatalog::discover_in(project.path(), Some(home.path()), &tools(), &[]);
        assert!(catalog.diagnostics.is_empty(), "{:?}", catalog.diagnostics);
        let child = &catalog.agents["focused"];
        assert_eq!(child.allowed_tools, vec!["read_file"]);
        assert_eq!(child.model.as_deref(), Some(model::PRO_MODEL));
        assert!(child.system_prompt.contains("Code Reviewer Role"));
        assert!(
            child
                .system_prompt
                .ends_with("Parent policy.\n\nChild policy.")
        );
        assert!(catalog.agents["empty"].allowed_tools.is_empty());
    }

    #[test]
    fn chinese_description_and_prompt_are_valid_utf8() {
        let text = "---\nname: audit\ndescription: \u{5ba1}\u{67e5}\u{4ee3}\u{7801}\n---\n\u{8bf7}\u{68c0}\u{67e5}\u{5b89}\u{5168}\u{95ee}\u{9898}\u{3002}\n";
        let definition = parse_definition(text, Path::new("audit.md"), &tools(), &[]).unwrap();
        assert_eq!(
            definition.header.description,
            "\u{5ba1}\u{67e5}\u{4ee3}\u{7801}"
        );
        assert_eq!(
            definition.body,
            "\u{8bf7}\u{68c0}\u{67e5}\u{5b89}\u{5168}\u{95ee}\u{9898}\u{3002}"
        );
    }

    #[test]
    fn invalid_project_override_blocks_user_by_declared_or_filename_identity() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        folder_trust::set_trust_with_config_dir(project.path(), home.path(), TrustLevel::Trusted)
            .unwrap();
        write_agent(
            &home.path().join("agents"),
            "audit.md",
            "audit",
            "",
            "User policy.",
        );
        let directory = project.path().join(".orca/agents");
        write_agent(
            &directory,
            "different.md",
            "audit",
            "tools: [unknown]\n",
            "Invalid.",
        );
        assert!(
            !AgentCatalog::discover_in(project.path(), Some(home.path()), &tools(), &[])
                .agents
                .contains_key("audit")
        );
        fs::remove_file(directory.join("different.md")).unwrap();
        fs::write(
            directory.join("audit.md"),
            "---\nname: [broken\n---\nInvalid.",
        )
        .unwrap();
        assert!(
            !AgentCatalog::discover_in(project.path(), Some(home.path()), &tools(), &[])
                .agents
                .contains_key("audit")
        );
    }

    #[test]
    fn cycles_unknown_parents_and_invalid_project_entries_are_diagnosed() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let directory = home.path().join("agents");
        write_agent(&directory, "a.md", "alpha", "extends: beta\n", "A.");
        write_agent(&directory, "b.md", "beta", "extends: alpha\n", "B.");
        write_agent(&directory, "c.md", "missing", "extends: absent\n", "C.");
        write_agent(&directory, "d.md", "general", "", "Reserved.");
        let catalog = AgentCatalog::discover_in(project.path(), Some(home.path()), &tools(), &[]);
        assert!(catalog.agents.is_empty());
        assert_eq!(catalog.diagnostics.len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_and_project_directories_are_not_loaded() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_agent(outside.path(), "audit.md", "audit", "", "Outside policy.");
        fs::create_dir(home.path().join("agents")).unwrap();
        symlink(
            outside.path().join("audit.md"),
            home.path().join("agents/audit.md"),
        )
        .unwrap();
        fs::create_dir(project.path().join(".orca")).unwrap();
        symlink(outside.path(), project.path().join(".orca/agents")).unwrap();
        folder_trust::set_trust_with_config_dir(project.path(), home.path(), TrustLevel::Trusted)
            .unwrap();
        let catalog = AgentCatalog::discover_in(project.path(), Some(home.path()), &tools(), &[]);
        assert!(catalog.agents.is_empty());
        assert_eq!(catalog.diagnostics.len(), 2);
    }
}
