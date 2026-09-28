use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capability::CapabilitySet;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    /// Streamable HTTP, or the legacy HTTP+SSE transport of protocol
    /// 2024-11-05 when the server turns the `initialize` POST away with 400,
    /// 404 or 405.
    Sse,
    /// Streamable HTTP.
    Http,
}

impl Default for McpTransportKind {
    fn default() -> Self {
        Self::Stdio
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransportKind,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    /// Explicit user-owned capabilities for this integration. The default is
    /// read-only; write/network/shell access must be granted deliberately.
    #[serde(default)]
    pub capabilities: CapabilitySet,
    #[serde(default)]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default)]
    pub tool_timeout_ms: Option<u64>,
    /// Name of the environment variable holding a bearer token for HTTP/SSE
    /// auth. Read at connect time; never stored in the config file itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    /// OAuth client id for the server's authorization flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
    /// Local loopback port the OAuth redirect callback listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_callback_port: Option<u16>,
    /// When set, only these server-declared tool names are registered; every
    /// other tool from this server is filtered out. `disabled_tools` is still
    /// applied on top of this allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Server-declared tool names to exclude from registration, applied
    /// after `enabled_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

/// Whether `tool_name` (the server's own, unsanitized tool name) should be
/// registered for `config`'s server: `enabled_tools`, when set, is an
/// allow-list (an empty list enables nothing); `disabled_tools` is then
/// subtracted from whatever the allow-list (or the absence of one) admits.
pub fn tool_is_enabled(config: &McpServerConfig, tool_name: &str) -> bool {
    let allowed = config
        .enabled_tools
        .as_ref()
        .is_none_or(|enabled| enabled.iter().any(|name| name == tool_name));
    let denied = config
        .disabled_tools
        .as_ref()
        .is_some_and(|disabled| disabled.iter().any(|name| name == tool_name));
    allowed && !denied
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResourceTemplate {
    pub server: String,
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub schema_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    /// Whether the server declared this tool read-only (`readOnlyHint`) and
    /// not destructive (`destructiveHint`). Governs whether the approval
    /// policy treats a call to it as a read or a write.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpToolRef {
    pub server: String,
    pub tool: String,
    pub schema_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolsListResult {
    #[serde(default)]
    pub tools: Vec<McpToolDescriptor>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ResourcesListResult {
    #[serde(default)]
    pub resources: Vec<McpResourceDescriptor>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ResourceTemplatesListResult {
    #[serde(rename = "resourceTemplates", default)]
    pub resource_templates: Vec<McpResourceTemplateDescriptor>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpResourceTemplateDescriptor {
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct McpToolAnnotations {
    #[serde(rename = "readOnlyHint", default)]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", default)]
    pub destructive_hint: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: Option<McpToolAnnotations>,
}

impl McpToolDescriptor {
    /// A server declares a tool read-only with `readOnlyHint`; a
    /// `destructiveHint` overrides that even when both are set, so the tool
    /// is still asked about like any other write.
    pub fn is_read_only(&self) -> bool {
        self.annotations.as_ref().is_some_and(|annotations| {
            annotations.read_only_hint == Some(true) && annotations.destructive_hint != Some(true)
        })
    }
}

/// Extracts the server name from an MCP tool's runtime name, which the
/// registry builds as `mcp__<server>__<tool>` (see
/// `orca_mcp::client::connect_server`): the segment after `mcp__` and before
/// the next `__`. Returns `None` when `tool` does not have that shape, or
/// when the server or tool segment is empty.
pub fn mcp_tool_server(tool: &str) -> Option<&str> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, local_tool) = rest.split_once("__")?;
    if server.is_empty() || local_tool.is_empty() {
        return None;
    }
    Some(server)
}

fn default_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadResourceResult {
    #[serde(default)]
    pub contents: Vec<McpResourceContent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpContent {
    Text {
        text: String,
    },
    // Both fields default so a block missing one still parses; `tool_image`
    // then rejects it with a note instead of the whole result failing.
    Image {
        #[serde(default)]
        data: String,
        #[serde(default, rename = "mimeType")]
        mime_type: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_needs_the_hint_and_no_destructive_hint() {
        let read_only: McpToolDescriptor =
            serde_json::from_str(r#"{"name":"a","annotations":{"readOnlyHint":true}}"#)
                .expect("descriptor with readOnlyHint");
        assert!(read_only.is_read_only());

        let destructive: McpToolDescriptor = serde_json::from_str(
            r#"{"name":"a","annotations":{"readOnlyHint":true,"destructiveHint":true}}"#,
        )
        .expect("descriptor with readOnlyHint and destructiveHint");
        assert!(!destructive.is_read_only());

        let no_annotations: McpToolDescriptor =
            serde_json::from_str(r#"{"name":"a"}"#).expect("descriptor without annotations");
        assert!(!no_annotations.is_read_only());

        let hint_false: McpToolDescriptor =
            serde_json::from_str(r#"{"name":"a","annotations":{"readOnlyHint":false}}"#)
                .expect("descriptor with readOnlyHint false");
        assert!(!hint_false.is_read_only());
    }

    #[test]
    fn mcp_tool_server_reads_the_server_segment() {
        assert_eq!(mcp_tool_server("mcp__github__create_issue"), Some("github"));
        assert_eq!(mcp_tool_server("mcp__a__b__c"), Some("a"));
        assert_eq!(mcp_tool_server("mcp__github"), None);
        assert_eq!(mcp_tool_server("mcp____x"), None);
        assert_eq!(mcp_tool_server("bash"), None);
    }

    #[test]
    fn tool_filters_apply_the_allow_list_then_the_deny_list() {
        let unfiltered = McpServerConfig::default();
        assert!(tool_is_enabled(&unfiltered, "a"));
        assert!(tool_is_enabled(&unfiltered, "b"));
        assert!(tool_is_enabled(&unfiltered, "c"));

        let allow_only = McpServerConfig {
            enabled_tools: Some(vec!["a".to_string(), "b".to_string()]),
            ..Default::default()
        };
        assert!(tool_is_enabled(&allow_only, "a"));
        assert!(tool_is_enabled(&allow_only, "b"));
        assert!(!tool_is_enabled(&allow_only, "c"));

        let allow_then_deny = McpServerConfig {
            enabled_tools: Some(vec!["a".to_string(), "b".to_string()]),
            disabled_tools: Some(vec!["b".to_string()]),
            ..Default::default()
        };
        assert!(tool_is_enabled(&allow_then_deny, "a"));
        assert!(!tool_is_enabled(&allow_then_deny, "b"));
        assert!(!tool_is_enabled(&allow_then_deny, "c"));

        let deny_only = McpServerConfig {
            disabled_tools: Some(vec!["c".to_string()]),
            ..Default::default()
        };
        assert!(tool_is_enabled(&deny_only, "a"));
        assert!(tool_is_enabled(&deny_only, "b"));
        assert!(!tool_is_enabled(&deny_only, "c"));
    }

    #[test]
    fn new_server_fields_round_trip_and_stay_absent_when_unset() {
        let toml_source = r#"
name = "custom"
transport = "http"
url = "https://example.test/mcp"
bearer_token_env_var = "EXAMPLE_TOKEN"
oauth_client_id = "client-123"
oauth_callback_port = 51000
enabled_tools = ["a", "b"]
disabled_tools = ["b"]
"#;
        let config: McpServerConfig =
            toml::from_str(toml_source).expect("parse http transport config with new fields");
        assert_eq!(config.transport, McpTransportKind::Http);
        assert_eq!(
            config.bearer_token_env_var.as_deref(),
            Some("EXAMPLE_TOKEN")
        );
        assert_eq!(config.oauth_client_id.as_deref(), Some("client-123"));
        assert_eq!(config.oauth_callback_port, Some(51000));
        assert_eq!(
            config.enabled_tools,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(config.disabled_tools, Some(vec!["b".to_string()]));

        let minimal = McpServerConfig {
            name: "demo".to_string(),
            ..Default::default()
        };
        let serialized = toml::to_string(&minimal).expect("serialize minimal config");
        for key in [
            "bearer_token_env_var",
            "oauth_client_id",
            "oauth_callback_port",
            "enabled_tools",
            "disabled_tools",
        ] {
            assert!(
                !serialized.contains(key),
                "serialized config unexpectedly contains {key}: {serialized}"
            );
        }
    }
}
