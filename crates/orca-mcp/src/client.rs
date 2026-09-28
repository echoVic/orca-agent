use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde_json::Value;

use crate::auth::is_auth_required;
use crate::legacy_sse::MCP_SSE_EVENT_STREAM_CLOSED;
use crate::transport::{self, McpElicitationHandler, McpTransport};
use orca_core::config::mcp_credentials::mcp_credentials_path;
use orca_core::conversation::ImageInput;
use orca_core::mcp_types::{
    CallToolResult, McpContent, McpResource, McpResourceTemplate, McpServerConfig, McpTool,
    McpToolRef, McpTransportKind, ReadResourceResult, ResourceTemplatesListResult,
    ResourcesListResult, ToolsListResult, tool_is_enabled,
};
use orca_core::tool_images::tool_image;

/// The MCP servers of a session and their tools. Clones share one registry,
/// so a server reconnected through any clone serves every holder's next
/// call.
#[derive(Clone, Default)]
pub struct McpRegistry {
    inner: Arc<RwLock<McpRegistryInner>>,
}

/// How a configured MCP server stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpServerState {
    /// Connected, with its tools registered.
    Ready,
    /// The last attempt to connect failed.
    Failed { message: String },
    /// The server wants the user to log in (`orca mcp login`).
    NeedsLogin,
    /// Turned off in the config, so never connected.
    Disabled,
}

#[derive(Clone, Default)]
struct McpRegistryInner {
    /// Every configured server that has a name, in config order.
    servers: Vec<McpServerEntry>,
    clients: HashMap<String, Arc<McpClient>>,
    /// The tools of every server, in config order.
    tools: Vec<McpTool>,
    lookup: HashMap<String, McpToolRef>,
    /// Problems with the config itself, such as a server without a name.
    errors: Vec<String>,
    /// Where a connection looks for a stored OAuth login.
    credentials_path: Option<PathBuf>,
}

#[derive(Clone)]
struct McpServerEntry {
    /// The canonical name, as in its tools' names.
    name: String,
    config: McpServerConfig,
    state: McpServerState,
    /// Its registered tools.
    tools: Vec<McpTool>,
    /// What went wrong when it was last connected: the failure, or tools
    /// left out because their names were taken.
    errors: Vec<String>,
}

impl McpRegistryInner {
    /// Rebuilds the registry-wide tool list and lookup from the servers'.
    fn index_tools(&mut self) {
        self.tools = self
            .servers
            .iter()
            .flat_map(|server| server.tools.iter().cloned())
            .collect();
        self.lookup = tool_lookup(&self.tools);
    }
}

struct McpClient {
    config: McpServerConfig,
    server_name: String,
    capabilities: McpServerCapabilities,
    transport: Mutex<Box<dyn McpTransport>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct McpServerCapabilities {
    resources: bool,
}

impl McpServerCapabilities {
    fn from_initialize_result(value: &Value) -> Self {
        Self {
            resources: value
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("resources"))
                .is_some(),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn resource_capable_for_test() -> Self {
        Self { resources: true }
    }
}

#[derive(Clone, Debug, Default)]
pub struct McpResourceListing {
    pub resources: Vec<McpResource>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct McpResourceTemplateListing {
    pub resource_templates: Vec<McpResourceTemplate>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpRequestError {
    Cancelled,
    Failed(String),
}

impl McpRequestError {
    fn from_message(message: String) -> Self {
        if message == MCP_TOOL_CALL_CANCELLED {
            Self::Cancelled
        } else {
            Self::Failed(message)
        }
    }
}

impl fmt::Display for McpRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str(MCP_TOOL_CALL_CANCELLED),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for McpRequestError {}

impl From<String> for McpRequestError {
    fn from(message: String) -> Self {
        Self::from_message(message)
    }
}

pub fn initialize_registry(configs: &[McpServerConfig]) -> McpRegistry {
    initialize_registry_with_credentials(configs, mcp_credentials_path())
}

/// Connects as [`initialize_registry`] does, reading stored OAuth logins
/// from `credentials_path`.
pub(crate) fn initialize_registry_with_credentials(
    configs: &[McpServerConfig],
    credentials_path: Option<PathBuf>,
) -> McpRegistry {
    let mut inner = McpRegistryInner {
        credentials_path,
        ..Default::default()
    };
    // Each name belongs to its first enabled server, or else to its first
    // disabled one.
    let mut owners: HashMap<String, usize> = HashMap::new();
    for (index, config) in configs.iter().enumerate() {
        let server_name = sanitize_name(&config.name);
        if server_name.is_empty() {
            continue;
        }
        let owner = owners.entry(server_name).or_insert(index);
        if configs[*owner].disabled && !config.disabled {
            *owner = index;
        }
    }
    let mut taken = HashSet::new();
    for (index, config) in configs.iter().enumerate() {
        let server_name = sanitize_name(&config.name);
        if server_name.is_empty() {
            if !config.disabled {
                inner
                    .errors
                    .push("skipping MCP server with empty name".to_string());
            }
            continue;
        }
        if owners.get(&server_name) != Some(&index) {
            if !config.disabled {
                inner.errors.push(format!(
                    "MCP server name conflict: '{server_name}' already registered, skipping '{}'",
                    config.name
                ));
            }
            continue;
        }
        let mut server = McpServerEntry {
            name: server_name,
            config: config.clone(),
            state: McpServerState::Disabled,
            tools: Vec::new(),
            errors: Vec::new(),
        };
        if !config.disabled {
            match connect_server(config, &server.name, inner.credentials_path.clone()) {
                Ok((client, tools)) => {
                    server.state = McpServerState::Ready;
                    server.tools =
                        take_tool_names(&server.name, tools, &mut taken, &mut server.errors);
                    inner.clients.insert(server.name.clone(), Arc::new(client));
                }
                Err(error) => {
                    server.state = failed_state(&error);
                    server.errors.push(error);
                }
            }
        }
        inner.servers.push(server);
    }
    inner.index_tools();
    McpRegistry::from_inner(inner)
}

/// Keeps the tools whose names are not `taken` yet, and takes them. Each
/// one left out is reported in `errors`.
fn take_tool_names(
    server_name: &str,
    tools: Vec<McpTool>,
    taken: &mut HashSet<String>,
    errors: &mut Vec<String>,
) -> Vec<McpTool> {
    tools
        .into_iter()
        .filter(|tool| {
            let free = taken.insert(tool.schema_name.clone());
            if !free {
                errors.push(format!(
                    "MCP tool name conflict: '{}' already registered, skipping from '{}'",
                    tool.schema_name, server_name
                ));
            }
            free
        })
        .collect()
}

fn tool_lookup(tools: &[McpTool]) -> HashMap<String, McpToolRef> {
    tools
        .iter()
        .map(|tool| {
            (
                tool.schema_name.clone(),
                McpToolRef {
                    server: tool.server.clone(),
                    tool: tool.name.clone(),
                    schema_name: tool.schema_name.clone(),
                },
            )
        })
        .collect()
}

/// The state of a server that could not be connected.
fn failed_state(error: &str) -> McpServerState {
    if is_auth_required(error) {
        McpServerState::NeedsLogin
    } else {
        McpServerState::Failed {
            message: error.to_string(),
        }
    }
}

fn connect_server(
    config: &McpServerConfig,
    server_name: &str,
    credentials_path: Option<PathBuf>,
) -> Result<(McpClient, Vec<McpTool>), String> {
    let transport = transport::connect_with_credentials(config, credentials_path)?;
    connect_server_with_transport(config, server_name, transport)
}

fn connect_server_with_transport(
    config: &McpServerConfig,
    server_name: &str,
    transport: Box<dyn McpTransport>,
) -> Result<(McpClient, Vec<McpTool>), String> {
    let initialize_result = transport.initialize()?;
    let capabilities = McpServerCapabilities::from_initialize_result(&initialize_result);
    let result = transport.list_tools()?;
    let list: ToolsListResult = serde_json::from_value(result)
        .map_err(|error| format!("invalid tools/list result for '{server_name}': {error}"))?;

    let tools = list
        .tools
        .into_iter()
        .filter(|tool| tool_is_enabled(config, &tool.name))
        .map(|tool| {
            let tool_name = sanitize_name(&tool.name);
            let read_only = tool.is_read_only();
            McpTool {
                server: server_name.to_string(),
                name: tool.name,
                schema_name: format!("mcp__{server_name}__{tool_name}"),
                description: tool.description,
                input_schema: normalize_schema(tool.input_schema),
                read_only,
            }
        })
        .collect();

    Ok((
        McpClient {
            config: config.clone(),
            server_name: server_name.to_string(),
            capabilities,
            transport: Mutex::new(transport),
        },
        tools,
    ))
}

impl McpRegistry {
    fn from_inner(inner: McpRegistryInner) -> Self {
        Self {
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    /// The registry as it stands. Never hold the guard across a request to a
    /// server.
    fn read(&self) -> RwLockReadGuard<'_, McpRegistryInner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, McpRegistryInner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The connected client of `server`.
    fn client(&self, server: &str) -> Option<Arc<McpClient>> {
        self.read().clients.get(server).cloned()
    }

    /// The connected clients that declared resources.
    fn resource_clients(&self) -> Vec<(String, Arc<McpClient>)> {
        self.read()
            .clients
            .iter()
            .filter(|(_, client)| client.capabilities.resources)
            .map(|(name, client)| (name.clone(), Arc::clone(client)))
            .collect()
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn from_tools_for_test(tools: Vec<McpTool>) -> Self {
        let lookup = tool_lookup(&tools);
        Self::from_inner(McpRegistryInner {
            tools,
            lookup,
            ..Default::default()
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn from_static_resources_for_test(
        resources: Vec<McpResource>,
        reads: HashMap<(String, String), ReadResourceResult>,
    ) -> Self {
        struct StaticResourceTransport {
            server: String,
            resources: Vec<McpResource>,
            reads: HashMap<(String, String), ReadResourceResult>,
        }

        impl McpTransport for StaticResourceTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("static resource transport does not support tool calls".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                let resources = self
                    .resources
                    .iter()
                    .map(|resource| {
                        serde_json::json!({
                            "uri": resource.uri,
                            "name": resource.name,
                            "description": resource.description,
                            "mimeType": resource.mime_type,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "resources": resources }))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({ "resourceTemplates": [] }))
            }

            fn read_resource(&self, uri: &str) -> Result<Value, String> {
                let content = self
                    .reads
                    .get(&(self.server.clone(), uri.to_string()))
                    .ok_or_else(|| format!("resource not found: {uri}"))?;
                serde_json::to_value(content).map_err(|error| error.to_string())
            }
        }

        let mut clients = HashMap::new();
        let mut grouped: HashMap<String, Vec<McpResource>> = HashMap::new();
        for resource in resources {
            grouped
                .entry(resource.server.clone())
                .or_default()
                .push(resource);
        }

        for (server, resources) in grouped {
            clients.insert(
                server.clone(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: server.clone(),
                        ..Default::default()
                    },
                    server_name: server.clone(),
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    transport: Mutex::new(Box::new(StaticResourceTransport {
                        server,
                        resources,
                        reads: reads.clone(),
                    })),
                }),
            );
        }

        Self::from_inner(McpRegistryInner {
            clients,
            ..Default::default()
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn from_resource_transports_for_test(
        transports: impl IntoIterator<Item = (String, Box<dyn McpTransport>)>,
    ) -> Self {
        let clients = transports
            .into_iter()
            .map(|(server, transport)| {
                (
                    server.clone(),
                    Arc::new(McpClient {
                        config: McpServerConfig {
                            name: server.clone(),
                            ..Default::default()
                        },
                        server_name: server,
                        capabilities: McpServerCapabilities::resource_capable_for_test(),
                        transport: Mutex::new(transport),
                    }),
                )
            })
            .collect();
        Self::from_inner(McpRegistryInner {
            clients,
            ..Default::default()
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_registry_errors_for_test(&self, errors: Vec<String>) -> Self {
        let mut inner = self.read().clone();
        for server in &mut inner.servers {
            server.errors.clear();
        }
        inner.errors = errors;
        Self::from_inner(inner)
    }

    /// The tools of every connected server, in config order.
    pub fn tools(&self) -> Vec<McpTool> {
        self.read().tools.clone()
    }

    /// What is wrong with the config, then what went wrong when each server
    /// was last connected.
    pub fn errors(&self) -> Vec<String> {
        let inner = self.read();
        inner
            .errors
            .iter()
            .chain(inner.servers.iter().flat_map(|server| &server.errors))
            .cloned()
            .collect()
    }

    /// Every configured server and how it stands, in config order. Servers
    /// that failed to connect and disabled servers are listed too.
    pub fn server_states(&self) -> Vec<(String, McpServerState)> {
        self.read()
            .servers
            .iter()
            .map(|server| (server.name.clone(), server.state.clone()))
            .collect()
    }

    /// Connects `name` again with its saved config, and replaces its client
    /// and tools with the new ones. When that fails, the server has no
    /// client or tools until it is reconnected, and its state says why.
    /// Other servers are left as they are.
    pub fn reconnect_server(&self, name: &str) -> Result<(), String> {
        let server_name = sanitize_name(name);
        let (config, credentials_path) = {
            let inner = self.read();
            let server = inner
                .servers
                .iter()
                .find(|server| server.name == server_name)
                .ok_or_else(|| format!("no MCP server named '{name}'"))?;
            if server.config.disabled {
                return Err(format!("MCP server '{name}' is disabled"));
            }
            (server.config.clone(), inner.credentials_path.clone())
        };
        // Connecting can take up to the startup timeout, so no lock is held.
        let connected = connect_server(&config, &server_name, credentials_path);

        let mut inner = self.write();
        let mut taken = inner
            .servers
            .iter()
            .filter(|server| server.name != server_name)
            .flat_map(|server| server.tools.iter().map(|tool| tool.schema_name.clone()))
            .collect::<HashSet<_>>();
        let Some(index) = inner
            .servers
            .iter()
            .position(|server| server.name == server_name)
        else {
            return Err(format!("no MCP server named '{name}'"));
        };
        let (result, replaced) = match connected {
            Ok((client, tools)) => {
                let server = &mut inner.servers[index];
                server.errors.clear();
                server.tools = take_tool_names(&server_name, tools, &mut taken, &mut server.errors);
                server.state = McpServerState::Ready;
                let replaced = inner.clients.insert(server_name, Arc::new(client));
                (Ok(()), replaced)
            }
            Err(error) => {
                let server = &mut inner.servers[index];
                server.state = failed_state(&error);
                server.tools.clear();
                server.errors = vec![error.clone()];
                (Err(error), inner.clients.remove(&server_name))
            }
        };
        inner.index_tools();
        drop(inner);
        // Dropping the old client stops its server, which may wait on the
        // process; the lock is released first.
        drop(replaced);
        result
    }

    pub fn resolve_tool(&self, schema_name: &str) -> Option<McpToolRef> {
        self.read().lookup.get(schema_name).cloned()
    }

    pub fn call_tool(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
    ) -> Result<McpCallOutput, String> {
        self.call_tool_inner(tool_ref, arguments, None, None)
    }

    pub fn call_tool_with_elicitation_handler(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<McpCallOutput, String> {
        self.call_tool_inner(tool_ref, arguments, handler, None)
    }

    pub fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpCallOutput, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.call_tool_inner(tool_ref, arguments, handler, Some(should_cancel))
    }

    pub fn call_tool_or_cancel(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpCallOutput, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.call_tool_inner(tool_ref, arguments, None, Some(should_cancel))
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn from_resource_listing_for_test(
        resources: Vec<McpResource>,
        errors: Vec<String>,
    ) -> Self {
        struct StaticResourceListingTransport {
            resources: Vec<McpResource>,
            error: Option<String>,
        }

        impl McpTransport for StaticResourceListingTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("static resource listing transport does not support tool calls".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                let resources = self
                    .resources
                    .iter()
                    .map(|resource| {
                        serde_json::json!({
                            "uri": resource.uri,
                            "name": resource.name,
                            "description": resource.description,
                            "mimeType": resource.mime_type,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "resources": resources }))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({ "resourceTemplates": [] }))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Ok(serde_json::json!({"contents": []}))
            }
        }

        let mut clients = HashMap::new();
        let mut grouped: HashMap<String, Vec<McpResource>> = HashMap::new();
        for resource in resources {
            grouped
                .entry(resource.server.clone())
                .or_default()
                .push(resource);
        }
        for (server, resources) in grouped {
            clients.insert(
                server.clone(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: server.clone(),
                        ..Default::default()
                    },
                    server_name: server,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    transport: Mutex::new(Box::new(StaticResourceListingTransport {
                        resources,
                        error: None,
                    })),
                }),
            );
        }
        for error in &errors {
            let server = error
                .split_once(':')
                .map(|(server, _)| server.trim())
                .filter(|server| !server.is_empty())
                .unwrap_or("error")
                .to_string();
            clients.insert(
                server.clone(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: server.clone(),
                        ..Default::default()
                    },
                    server_name: server,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    transport: Mutex::new(Box::new(StaticResourceListingTransport {
                        resources: Vec::new(),
                        error: Some(
                            error
                                .split_once(':')
                                .map(|(_, message)| message.trim().to_string())
                                .unwrap_or_else(|| error.clone()),
                        ),
                    })),
                }),
            );
        }

        Self::from_inner(McpRegistryInner {
            clients,
            ..Default::default()
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn from_resource_template_listing_for_test(
        resource_templates: Vec<McpResourceTemplate>,
        errors: Vec<String>,
    ) -> Self {
        struct StaticResourceTemplateListingTransport {
            resource_templates: Vec<McpResourceTemplate>,
            error: Option<String>,
        }

        impl McpTransport for StaticResourceTemplateListingTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err(
                    "static resource template listing transport does not support tool calls"
                        .to_string(),
                )
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({ "resources": [] }))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                let resource_templates = self
                    .resource_templates
                    .iter()
                    .map(|template| {
                        serde_json::json!({
                            "uriTemplate": template.uri_template,
                            "name": template.name,
                            "description": template.description,
                            "mimeType": template.mime_type,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "resourceTemplates": resource_templates }))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Ok(serde_json::json!({"contents": []}))
            }
        }

        let mut clients = HashMap::new();
        let mut grouped: HashMap<String, Vec<McpResourceTemplate>> = HashMap::new();
        for template in resource_templates {
            grouped
                .entry(template.server.clone())
                .or_default()
                .push(template);
        }
        for (server, resource_templates) in grouped {
            clients.insert(
                server.clone(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: server.clone(),
                        ..Default::default()
                    },
                    server_name: server,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    transport: Mutex::new(Box::new(StaticResourceTemplateListingTransport {
                        resource_templates,
                        error: None,
                    })),
                }),
            );
        }
        for error in &errors {
            let server = error
                .split_once(':')
                .map(|(server, _)| server.trim())
                .filter(|server| !server.is_empty())
                .unwrap_or("error")
                .to_string();
            clients.insert(
                server.clone(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: server.clone(),
                        ..Default::default()
                    },
                    server_name: server,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    transport: Mutex::new(Box::new(StaticResourceTemplateListingTransport {
                        resource_templates: Vec::new(),
                        error: Some(
                            error
                                .split_once(':')
                                .map(|(_, message)| message.trim().to_string())
                                .unwrap_or_else(|| error.clone()),
                        ),
                    })),
                }),
            );
        }

        Self::from_inner(McpRegistryInner {
            clients,
            ..Default::default()
        })
    }

    fn call_tool_inner(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
        elicitation_handler: Option<&dyn McpElicitationHandler>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<McpCallOutput, String> {
        let client = self
            .client(&tool_ref.server)
            .ok_or_else(|| format!("MCP server '{}' is not connected", tool_ref.server))?;
        let result = client.call_tool(
            &tool_ref.tool,
            arguments,
            elicitation_handler,
            should_cancel,
        )?;
        let result: CallToolResult = serde_json::from_value(result)
            .map_err(|error| format!("invalid MCP tool result: {error}"))?;

        let mut texts = Vec::new();
        let mut rejected_notes = Vec::new();
        let mut images = Vec::new();
        for content in result.content {
            match content {
                McpContent::Text { text } if text.is_empty() => {}
                McpContent::Text { text } => texts.push(text),
                McpContent::Image { data, mime_type } => match tool_image(&mime_type, data) {
                    Ok(image) => images.push(image),
                    Err(rejected) => rejected_notes.push(rejected.note()),
                },
                McpContent::Other => {}
            }
        }

        let mut parts = Vec::new();
        if !texts.is_empty() {
            parts.push(texts.join("\n"));
        }
        parts.extend(rejected_notes);
        if !images.is_empty() {
            parts.push(if images.len() == 1 {
                "[1 image attached]".to_string()
            } else {
                format!("[{} images attached]", images.len())
            });
        }

        Ok(McpCallOutput {
            output: if parts.is_empty() {
                "(MCP tool returned no text content)".to_string()
            } else {
                parts.join("\n")
            },
            images,
            is_error: result.is_error,
        })
    }

    pub fn list_resources(&self, server: Option<&str>) -> Result<Vec<McpResource>, String> {
        self.list_resources_or_cancel(server, &|| false)
            .map_err(|error| error.to_string())
    }

    pub fn list_resources_or_cancel(
        &self,
        server: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<McpResource>, McpRequestError> {
        let clients = match server {
            Some(server) => vec![(
                server.to_string(),
                self.client(server).ok_or_else(|| {
                    McpRequestError::Failed(format!("MCP server '{server}' is not connected"))
                })?,
            )],
            None => self.resource_clients(),
        };

        let mut resources = Vec::new();
        for (server, client) in clients {
            let result = client.list_resources_or_cancel(should_cancel)?;
            let result: ResourcesListResult = serde_json::from_value(result).map_err(|error| {
                McpRequestError::Failed(format!("invalid MCP resources/list result: {error}"))
            })?;
            resources.extend(result.resources.into_iter().map(|resource| McpResource {
                server: server.clone(),
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime_type: resource.mime_type,
            }));
        }

        Ok(resources)
    }

    pub fn list_resources_with_errors(&self, server: Option<&str>) -> McpResourceListing {
        self.list_resources_with_errors_or_cancel(server, &|| false)
            .unwrap_or_else(|error| McpResourceListing {
                resources: Vec::new(),
                errors: vec![error.to_string()],
            })
    }

    pub fn list_resources_with_errors_or_cancel(
        &self,
        server: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpResourceListing, McpRequestError> {
        let clients = match server {
            Some(server) => match self.client(server) {
                Some(client) => vec![(server.to_string(), client)],
                None => {
                    return Ok(McpResourceListing {
                        resources: Vec::new(),
                        errors: vec![format!("MCP server '{server}' is not connected")],
                    });
                }
            },
            None => self.resource_clients(),
        };

        let mut listing = McpResourceListing {
            resources: Vec::new(),
            errors: if server.is_none() {
                self.errors()
            } else {
                Vec::new()
            },
        };
        for (server, client) in clients {
            match client.list_resources_or_cancel(should_cancel) {
                Ok(result) => match serde_json::from_value::<ResourcesListResult>(result) {
                    Ok(result) => {
                        listing
                            .resources
                            .extend(result.resources.into_iter().map(|resource| McpResource {
                                server: server.clone(),
                                uri: resource.uri,
                                name: resource.name,
                                description: resource.description,
                                mime_type: resource.mime_type,
                            }));
                    }
                    Err(error) => listing.errors.push(format!(
                        "{server}: invalid MCP resources/list result: {error}"
                    )),
                },
                Err(McpRequestError::Cancelled) => return Err(McpRequestError::Cancelled),
                Err(McpRequestError::Failed(error)) => {
                    listing.errors.push(format!("{server}: {error}"));
                }
            }
        }

        Ok(listing)
    }

    pub fn list_resource_templates(
        &self,
        server: Option<&str>,
    ) -> Result<Vec<McpResourceTemplate>, String> {
        self.list_resource_templates_or_cancel(server, &|| false)
            .map_err(|error| error.to_string())
    }

    pub fn list_resource_templates_or_cancel(
        &self,
        server: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<McpResourceTemplate>, McpRequestError> {
        let clients = match server {
            Some(server) => vec![(
                server.to_string(),
                self.client(server).ok_or_else(|| {
                    McpRequestError::Failed(format!("MCP server '{server}' is not connected"))
                })?,
            )],
            None => self.resource_clients(),
        };

        let mut resource_templates = Vec::new();
        for (server, client) in clients {
            let result = client.list_resource_templates_or_cancel(should_cancel)?;
            let result: ResourceTemplatesListResult =
                serde_json::from_value(result).map_err(|error| {
                    McpRequestError::Failed(format!(
                        "invalid MCP resources/templates/list result: {error}"
                    ))
                })?;
            resource_templates.extend(result.resource_templates.into_iter().map(|template| {
                McpResourceTemplate {
                    server: server.clone(),
                    uri_template: template.uri_template,
                    name: template.name,
                    description: template.description,
                    mime_type: template.mime_type,
                }
            }));
        }

        Ok(resource_templates)
    }

    pub fn list_resource_templates_with_errors(
        &self,
        server: Option<&str>,
    ) -> McpResourceTemplateListing {
        self.list_resource_templates_with_errors_or_cancel(server, &|| false)
            .unwrap_or_else(|error| McpResourceTemplateListing {
                resource_templates: Vec::new(),
                errors: vec![error.to_string()],
            })
    }

    pub fn list_resource_templates_with_errors_or_cancel(
        &self,
        server: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpResourceTemplateListing, McpRequestError> {
        let clients = match server {
            Some(server) => match self.client(server) {
                Some(client) => vec![(server.to_string(), client)],
                None => {
                    return Ok(McpResourceTemplateListing {
                        resource_templates: Vec::new(),
                        errors: vec![format!("MCP server '{server}' is not connected")],
                    });
                }
            },
            None => self.resource_clients(),
        };

        let mut listing = McpResourceTemplateListing {
            resource_templates: Vec::new(),
            errors: if server.is_none() {
                self.errors()
            } else {
                Vec::new()
            },
        };
        for (server, client) in clients {
            match client.list_resource_templates_or_cancel(should_cancel) {
                Ok(result) => match serde_json::from_value::<ResourceTemplatesListResult>(result) {
                    Ok(result) => {
                        listing.resource_templates.extend(
                            result.resource_templates.into_iter().map(|template| {
                                McpResourceTemplate {
                                    server: server.clone(),
                                    uri_template: template.uri_template,
                                    name: template.name,
                                    description: template.description,
                                    mime_type: template.mime_type,
                                }
                            }),
                        );
                    }
                    Err(error) => listing.errors.push(format!(
                        "{server}: invalid MCP resources/templates/list result: {error}"
                    )),
                },
                Err(McpRequestError::Cancelled) => return Err(McpRequestError::Cancelled),
                Err(McpRequestError::Failed(error)) => {
                    listing.errors.push(format!("{server}: {error}"));
                }
            }
        }

        Ok(listing)
    }

    pub fn read_resource(&self, server: &str, uri: &str) -> Result<ReadResourceResult, String> {
        self.read_resource_or_cancel(server, uri, &|| false)
            .map_err(|error| error.to_string())
    }

    pub fn read_resource_or_cancel(
        &self,
        server: &str,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<ReadResourceResult, McpRequestError> {
        let client = self.client(server).ok_or_else(|| {
            McpRequestError::Failed(format!("MCP server '{server}' is not connected"))
        })?;
        let result = client.read_resource_or_cancel(uri, should_cancel)?;
        serde_json::from_value(result).map_err(|error| {
            McpRequestError::Failed(format!("invalid MCP resources/read result: {error}"))
        })
    }
}

impl McpClient {
    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        elicitation_handler: Option<&dyn McpElicitationHandler>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, String> {
        match self.call_tool_once(name, arguments, elicitation_handler, should_cancel) {
            Err(error) if should_reconnect_after_mcp_error(&self.config.transport, &error) => {
                let startup_timeout_cap_ms = (self.config.transport == McpTransportKind::Stdio
                    && error == MCP_TOOL_CALL_CANCELLED)
                    .then_some(CANCELLED_STDIO_RECONNECT_TIMEOUT_MS);
                let _ = self.reconnect(startup_timeout_cap_ms);
                Err(error)
            }
            result => result,
        }
    }

    fn call_tool_once(
        &self,
        name: &str,
        arguments: Value,
        elicitation_handler: Option<&dyn McpElicitationHandler>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, String> {
        let transport = self
            .transport
            .lock()
            .map_err(|_| format!("MCP server '{}' transport lock poisoned", self.server_name))?;
        match should_cancel {
            Some(should_cancel) => transport.call_tool_with_elicitation_handler_or_cancel(
                name,
                arguments,
                elicitation_handler,
                should_cancel,
            ),
            None => {
                transport.call_tool_with_elicitation_handler(name, arguments, elicitation_handler)
            }
        }
    }

    fn list_resources_or_cancel(
        &self,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, McpRequestError> {
        self.resource_request_or_cancel(|transport| {
            transport.list_resources_or_cancel(should_cancel)
        })
    }

    fn list_resource_templates_or_cancel(
        &self,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, McpRequestError> {
        self.resource_request_or_cancel(|transport| {
            transport.list_resource_templates_or_cancel(should_cancel)
        })
    }

    fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, McpRequestError> {
        self.resource_request_or_cancel(|transport| {
            transport.read_resource_or_cancel(uri, should_cancel)
        })
    }

    fn resource_request_or_cancel(
        &self,
        request: impl FnOnce(&dyn McpTransport) -> Result<Value, String>,
    ) -> Result<Value, McpRequestError> {
        let result = {
            let transport = self.transport.lock().map_err(|_| {
                McpRequestError::Failed(format!(
                    "MCP server '{}' transport lock poisoned",
                    self.server_name
                ))
            })?;
            request(transport.as_ref())
        };
        match result {
            Err(error) if should_reconnect_after_mcp_error(&self.config.transport, &error) => {
                let startup_timeout_cap_ms = (self.config.transport == McpTransportKind::Stdio
                    && error == MCP_TOOL_CALL_CANCELLED)
                    .then_some(CANCELLED_STDIO_RECONNECT_TIMEOUT_MS);
                let _ = self.reconnect(startup_timeout_cap_ms);
                Err(McpRequestError::from_message(error))
            }
            Ok(result) => Ok(result),
            Err(error) => Err(McpRequestError::from_message(error)),
        }
    }

    fn reconnect(&self, startup_timeout_cap_ms: Option<u64>) -> Result<(), String> {
        let mut config = self.config.clone();
        if let Some(cap_ms) = startup_timeout_cap_ms {
            config.startup_timeout_ms =
                Some(config.startup_timeout_ms.unwrap_or(cap_ms).min(cap_ms));
        }
        let transport = transport::connect(&config)?;
        transport.initialize()?;
        let _ = transport.list_tools()?;
        let mut current = self
            .transport
            .lock()
            .map_err(|_| format!("MCP server '{}' transport lock poisoned", self.server_name))?;
        *current = transport;
        Ok(())
    }
}

const MCP_TOOL_CALL_CANCELLED: &str = "MCP tool call cancelled";
const CANCELLED_STDIO_RECONNECT_TIMEOUT_MS: u64 = 500;

fn should_reconnect_after_mcp_error(transport: &McpTransportKind, error: &str) -> bool {
    error.contains("timed out")
        || (transport == &McpTransportKind::Stdio && error.contains("MCP tool call cancelled"))
        || error.contains("reader stopped")
        || error.contains("server closed stdout")
        || error.contains("failed to write MCP request")
        // An oversized stdio response stops the reader and kills the server.
        || error.contains("MCP response exceeded maximum line size")
        // The legacy SSE event stream ended; a new connection opens another.
        || error.contains(MCP_SSE_EVENT_STREAM_CLOSED)
}

#[derive(Debug)]
pub struct McpCallOutput {
    pub output: String,
    pub images: Vec<ImageInput>,
    pub is_error: bool,
}

fn normalize_schema(schema: Value) -> Value {
    if schema.get("type").is_some() {
        schema
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
}

fn sanitize_name(name: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_underscore = false;
    for ch in name.chars() {
        let next = if ch.is_ascii_alphanumeric() { ch } else { '_' };
        if next == '_' {
            if !last_was_underscore {
                sanitized.push(next);
            }
            last_was_underscore = true;
        } else {
            sanitized.push(next.to_ascii_lowercase());
            last_was_underscore = false;
        }
    }
    sanitized.trim_matches('_').to_string()
}

pub fn canonical_server_name(name: &str) -> String {
    sanitize_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{
        McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
        McpTransport,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const STDIO_TEST_STARTUP_TIMEOUT_MS: u64 = 15_000;

    #[cfg(unix)]
    fn stdio_fixture_config(name: &str, server: &std::path::Path) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![server.to_string_lossy().into_owned()],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(1000),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn stdio_fixture_config_runs_shell_script_through_sh() {
        let config = stdio_fixture_config("templates", std::path::Path::new("/tmp/mcp.sh"));

        assert_eq!(config.command.as_deref(), Some("/bin/sh"));
        assert_eq!(config.args, vec!["/tmp/mcp.sh"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_marked_read_only_by_its_server_is_read_only() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("annotated_tools_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"annotated","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read_file","description":"reads a file","inputSchema":{"type":"object","properties":{},"required":[]},"annotations":{"readOnlyHint":true}},{"name":"write_file","description":"writes a file","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let config = stdio_fixture_config("annotated", &server);

        let registry = initialize_registry(&[config]);

        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let tools = registry.tools();
        let read_tool = tools
            .iter()
            .find(|tool| tool.name == "read_file")
            .expect("read_file tool");
        let write_tool = tools
            .iter()
            .find(|tool| tool.name == "write_file")
            .expect("write_file tool");
        assert!(read_tool.read_only);
        assert!(!write_tool.read_only);
    }

    #[test]
    fn filtered_tools_are_not_registered() {
        struct FixedToolsTransport;

        impl McpTransport for FixedToolsTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": [
                    {"name": "a", "inputSchema": {"type": "object"}},
                    {"name": "b", "inputSchema": {"type": "object"}},
                    {"name": "c", "inputSchema": {"type": "object"}}
                ]}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("fixed tools transport does not support tool calls".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resources": []}))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resourceTemplates": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Err("fixed tools transport does not support resource reads".to_string())
            }
        }

        let config = McpServerConfig {
            name: "filtered".to_string(),
            disabled_tools: Some(vec!["b".to_string()]),
            ..Default::default()
        };

        let (client, tools) =
            connect_server_with_transport(&config, "filtered", Box::new(FixedToolsTransport))
                .expect("connect with fixed tools transport");
        let lookup = tools
            .iter()
            .map(|tool| {
                (
                    tool.schema_name.clone(),
                    McpToolRef {
                        server: tool.server.clone(),
                        tool: tool.name.clone(),
                        schema_name: tool.schema_name.clone(),
                    },
                )
            })
            .collect();
        let registry = McpRegistry::from_inner(McpRegistryInner {
            clients: HashMap::from([("filtered".to_string(), Arc::new(client))]),
            tools,
            lookup,
            ..Default::default()
        });

        let names: Vec<String> = registry.tools().into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn sanitizes_mcp_schema_names() {
        assert_eq!(sanitize_name("GitHub Files"), "github_files");
        assert_eq!(sanitize_name("search.repos"), "search_repos");
    }

    #[test]
    fn normalizes_non_object_schema() {
        let schema = normalize_schema(Value::Null);
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn tools_preserve_insertion_order() {
        // The DeepSeek tool schema is byte-pinned for prefix caching, so the
        // registry must expose tools in a stable order. `tools()` is backed by a
        // Vec (insertion order), never a HashMap; lock that here so a future
        // refactor to a map-backed store fails loudly.
        let make = |schema_name: &str| McpTool {
            server: "srv".to_string(),
            name: schema_name.to_string(),
            schema_name: schema_name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        };
        let order = vec!["mcp__srv__zzz", "mcp__srv__aaa", "mcp__srv__mmm"];
        let registry =
            McpRegistry::from_tools_for_test(order.iter().map(|n| make(n)).collect::<Vec<_>>());
        let got: Vec<String> = registry
            .tools()
            .into_iter()
            .map(|tool| tool.schema_name)
            .collect();
        assert_eq!(got, order);
    }

    #[test]
    fn call_tool_or_cancel_waits_for_transport_cleanup() {
        struct CleanupAwareTransport {
            active: Arc<AtomicBool>,
            release: Arc<AtomicBool>,
        }

        impl McpTransport for CleanupAwareTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                self.active.store(true, Ordering::SeqCst);
                while !self.release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.active.store(false, Ordering::SeqCst);
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": "too late"}],
                    "isError": false
                }))
            }

            fn call_tool_with_elicitation_handler_or_cancel(
                &self,
                _name: &str,
                _arguments: Value,
                _handler: Option<&dyn McpElicitationHandler>,
                should_cancel: &dyn Fn() -> bool,
            ) -> Result<Value, String> {
                self.active.store(true, Ordering::SeqCst);
                while !should_cancel() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.active.store(false, Ordering::SeqCst);
                Err("MCP tool call cancelled".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resources": []}))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resourceTemplates": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Ok(serde_json::json!({"contents": []}))
            }
        }

        let tool = McpTool {
            server: "slow".to_string(),
            name: "wait".to_string(),
            schema_name: "mcp__slow__wait".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        };
        let active = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let registry = McpRegistry::from_inner(McpRegistryInner {
            clients: HashMap::from([(
                "slow".to_string(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: "slow".to_string(),
                        ..Default::default()
                    },
                    server_name: "slow".to_string(),
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    transport: Mutex::new(Box::new(CleanupAwareTransport {
                        active: Arc::clone(&active),
                        release: Arc::clone(&release),
                    })),
                }),
            )]),
            tools: vec![tool.clone()],
            lookup: HashMap::from([(
                tool.schema_name.clone(),
                McpToolRef {
                    server: tool.server,
                    tool: tool.name,
                    schema_name: tool.schema_name,
                },
            )]),
            ..Default::default()
        });
        let tool_ref = registry
            .resolve_tool("mcp__slow__wait")
            .expect("tool ref for slow MCP tool");
        let started = Instant::now();

        let result =
            registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
                active.load(Ordering::SeqCst) && started.elapsed() >= Duration::from_millis(50)
            });

        let worker_active_at_return = active.load(Ordering::SeqCst);
        release.store(true, Ordering::SeqCst);
        let cleanup_deadline = Instant::now() + Duration::from_secs(1);
        while active.load(Ordering::SeqCst) && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(started.elapsed() < Duration::from_millis(750));
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert!(
            !worker_active_at_return,
            "call_tool_or_cancel returned before its transport worker finished"
        );
    }

    /// A tiny, valid 1x1 PNG (base64-encoded). Mirrors the constant
    /// `orca_core::tool_images`'s own tests define, per that module's note
    /// that each caller should define its own rather than share one.
    const BASE64_1X1_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    /// Builds a registry with a single MCP server ("screen") whose only tool
    /// ("capture") always returns `content` from `call_tool`.
    fn registry_with_call_tool_result(content: Value) -> (McpRegistry, McpToolRef) {
        struct StaticCallToolTransport {
            content: Value,
        }

        impl McpTransport for StaticCallToolTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Ok(self.content.clone())
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resources": []}))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resourceTemplates": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Ok(serde_json::json!({"contents": []}))
            }
        }

        let tool = McpTool {
            server: "screen".to_string(),
            name: "capture".to_string(),
            schema_name: "mcp__screen__capture".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        };
        let registry = McpRegistry::from_inner(McpRegistryInner {
            clients: HashMap::from([(
                "screen".to_string(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: "screen".to_string(),
                        ..Default::default()
                    },
                    server_name: "screen".to_string(),
                    capabilities: McpServerCapabilities::default(),
                    transport: Mutex::new(Box::new(StaticCallToolTransport { content })),
                }),
            )]),
            tools: vec![tool.clone()],
            lookup: HashMap::from([(
                tool.schema_name.clone(),
                McpToolRef {
                    server: tool.server,
                    tool: tool.name,
                    schema_name: tool.schema_name,
                },
            )]),
            ..Default::default()
        });
        let tool_ref = registry
            .resolve_tool("mcp__screen__capture")
            .expect("tool ref for screen capture");
        (registry, tool_ref)
    }

    #[test]
    fn an_image_from_an_mcp_tool_reaches_the_tool_output() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [
                {"type": "text", "text": "screenshot taken"},
                {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"}
            ],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("tool result");

        assert_eq!(result.output, "screenshot taken\n[1 image attached]");
        assert_eq!(result.images.len(), 1);
        assert!(matches!(
            &result.images[0].source,
            orca_core::conversation::ImageSource::Base64 { media_type, .. }
                if media_type == "image/png"
        ));
    }

    #[test]
    fn an_unsupported_mcp_image_leaves_a_note() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [
                {"type": "text", "text": "screenshot taken"},
                {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"}
            ],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("tool result");

        assert_eq!(
            result.output,
            "screenshot taken\n[image omitted: unsupported type image/svg+xml]"
        );
        assert!(result.images.is_empty());
    }

    #[test]
    fn an_mcp_image_without_a_media_type_leaves_a_note_instead_of_failing_the_call() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [
                {"type": "text", "text": "screenshot taken"},
                {"type": "image", "data": BASE64_1X1_PNG}
            ],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("a malformed image block must not fail the call");

        assert_eq!(
            result.output,
            "screenshot taken\n[image omitted: missing media type]"
        );
        assert!(result.images.is_empty());
    }

    #[test]
    fn a_tool_whose_only_text_block_is_empty_reports_no_text_content() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [{"type": "text", "text": ""}],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("tool result");

        assert_eq!(result.output, "(MCP tool returned no text content)");
    }

    #[test]
    fn two_accepted_mcp_images_share_one_plural_marker() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [
                {"type": "text", "text": "screenshots taken"},
                {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"},
                {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"}
            ],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("tool result");

        assert_eq!(result.output, "screenshots taken\n[2 images attached]");
        assert_eq!(result.images.len(), 2);
    }

    #[test]
    fn rejected_only_mcp_images_leave_only_their_notes() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [
                {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"},
                {"type": "image", "data": "not base64!", "mimeType": "image/png"}
            ],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("tool result");

        assert_eq!(
            result.output,
            "[image omitted: unsupported type image/svg+xml]\n[image omitted: invalid base64 data]"
        );
        assert!(result.images.is_empty());
    }

    #[test]
    fn mixed_mcp_content_lists_text_then_notes_then_the_image_marker() {
        let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
            "content": [
                {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"},
                {"type": "text", "text": "screenshot taken"},
                {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"}
            ],
            "isError": false
        }));

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("tool result");

        assert_eq!(
            result.output,
            "screenshot taken\n[image omitted: unsupported type image/svg+xml]\n[1 image attached]"
        );
        assert_eq!(result.images.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_stdio_tool_call_reconnects_before_returning() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("reconnecting_mcp_server.sh");
        let generation_file = temp_dir.path().join("generation");
        let pid_prefix = temp_dir.path().join("pid");
        let started_file = temp_dir.path().join("started");
        std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
printf '%s' "$$" > "$PID_PREFIX.$generation"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"reconnect","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf started > "$STARTED_FILE"
      if [ "$generation" -eq 1 ]; then
        IFS= read -r ignored
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"generation 2"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let mut config = stdio_fixture_config("reconnect", &server);
        config.env = HashMap::from([
            (
                "GENERATION_FILE".to_string(),
                generation_file.to_string_lossy().into_owned(),
            ),
            (
                "PID_PREFIX".to_string(),
                pid_prefix.to_string_lossy().into_owned(),
            ),
            (
                "STARTED_FILE".to_string(),
                started_file.to_string_lossy().into_owned(),
            ),
        ]);
        config.tool_timeout_ms = Some(500);
        let registry = initialize_registry(&[config]);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let tool_ref = registry
            .resolve_tool("mcp__reconnect__wait")
            .expect("reconnect tool ref");

        let result =
            registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
                started_file.exists()
            });

        let generation_at_return =
            std::fs::read_to_string(&generation_file).expect("generation at cancellation return");
        let first_pid =
            std::fs::read_to_string(pid_prefix.with_extension("1")).expect("first server pid");
        let first_pid_alive_at_return = process_is_alive(first_pid.trim());

        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        while (std::fs::read_to_string(&generation_file).ok().as_deref() != Some("2")
            || process_is_alive(first_pid.trim()))
            && Instant::now() < cleanup_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let second = registry
            .call_tool(&tool_ref, Value::Object(Default::default()))
            .expect("tool call after reconnect");

        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert_eq!(
            generation_at_return, "2",
            "reconnect must finish before return"
        );
        assert!(
            !first_pid_alive_at_return,
            "cancelled stdio server must be reaped before return"
        );
        assert_eq!(second.output, "generation 2");
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_stdio_resource_listing_reconnects_before_returning() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("reconnecting_resource_server.sh");
        let generation_file = temp_dir.path().join("resource-generation");
        let pid_prefix = temp_dir.path().join("resource-pid");
        let started_file = temp_dir.path().join("resource-started");
        std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
printf '%s' "$$" > "$PID_PREFIX.$generation"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resource-reconnect","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf started > "$STARTED_FILE"
      if [ "$generation" -eq 1 ]; then
        IFS= read -r ignored
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://generation-2","name":"generation 2"}]}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let mut config = stdio_fixture_config("resource_reconnect", &server);
        config.env = HashMap::from([
            (
                "GENERATION_FILE".to_string(),
                generation_file.to_string_lossy().into_owned(),
            ),
            (
                "PID_PREFIX".to_string(),
                pid_prefix.to_string_lossy().into_owned(),
            ),
            (
                "STARTED_FILE".to_string(),
                started_file.to_string_lossy().into_owned(),
            ),
        ]);
        config.tool_timeout_ms = Some(5_000);
        let registry = initialize_registry(&[config]);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let result = registry.list_resources_with_errors_or_cancel(None, &|| started_file.exists());

        let generation_at_return =
            std::fs::read_to_string(&generation_file).expect("generation at cancellation return");
        let first_pid =
            std::fs::read_to_string(pid_prefix.with_extension("1")).expect("first server pid");
        let first_pid_alive_at_return = process_is_alive(first_pid.trim());
        let second = registry
            .list_resources(Some("resource_reconnect"))
            .expect("resource listing after reconnect");

        assert_eq!(result.unwrap_err(), McpRequestError::Cancelled);
        assert_eq!(
            generation_at_return, "2",
            "resource reconnect must finish before return"
        );
        assert!(
            !first_pid_alive_at_return,
            "cancelled resource server must be reaped before return"
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].uri, "memo://generation-2");
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_stdio_tool_call_bounds_failed_reconnect_handshake() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("stalled_reconnect_mcp_server.sh");
        let generation_file = temp_dir.path().join("generation");
        let pid_prefix = temp_dir.path().join("pid");
        let started_file = temp_dir.path().join("started");
        std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
printf '%s' "$$" > "$PID_PREFIX.$generation"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      if [ "$generation" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"reconnect","version":"1"}}}\n'
      else
        sleep 10
      fi
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf started > "$STARTED_FILE"
      IFS= read -r ignored
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let mut config = stdio_fixture_config("stalled_reconnect", &server);
        config.env = HashMap::from([
            (
                "GENERATION_FILE".to_string(),
                generation_file.to_string_lossy().into_owned(),
            ),
            (
                "PID_PREFIX".to_string(),
                pid_prefix.to_string_lossy().into_owned(),
            ),
            (
                "STARTED_FILE".to_string(),
                started_file.to_string_lossy().into_owned(),
            ),
        ]);
        config.startup_timeout_ms = Some(3_000);
        let registry = initialize_registry(&[config]);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let tool_ref = registry
            .resolve_tool("mcp__stalled_reconnect__wait")
            .expect("stalled reconnect tool ref");
        let started = Instant::now();

        let result =
            registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
                started_file.exists()
            });

        let elapsed = started.elapsed();
        let generation_at_return =
            std::fs::read_to_string(&generation_file).expect("generation at cancellation return");
        let second_pid =
            std::fs::read_to_string(pid_prefix.with_extension("2")).expect("second server pid");

        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert_eq!(generation_at_return, "2", "reconnect must be attempted");
        assert!(
            elapsed < Duration::from_secs(2),
            "failed reconnect delayed cancellation for {elapsed:?}"
        );
        assert!(
            !process_is_alive(second_pid.trim()),
            "failed reconnect server must be reaped before return"
        );
    }

    #[cfg(unix)]
    fn process_is_alive(pid: &str) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn cancellation_reconnect_policy_is_transport_aware() {
        assert!(should_reconnect_after_mcp_error(
            &McpTransportKind::Stdio,
            "MCP tool call cancelled"
        ));
        assert!(!should_reconnect_after_mcp_error(
            &McpTransportKind::Sse,
            "MCP tool call cancelled"
        ));
    }

    #[test]
    fn registry_call_tool_routes_elicitation_handler_to_transport() {
        struct ElicitingTransport;

        impl McpTransport for ElicitingTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("handler-aware call expected".to_string())
            }

            fn call_tool_with_elicitation_handler(
                &self,
                _name: &str,
                _arguments: Value,
                handler: Option<&dyn McpElicitationHandler>,
            ) -> Result<Value, String> {
                let response = handler.expect("elicitation handler").handle_elicitation(
                    McpElicitationRequest {
                        server_name: "prompts".to_string(),
                        id: "prompt-1".to_string(),
                        mode: McpElicitationMode::Form,
                        message: "Enter token".to_string(),
                        url: None,
                        requested_schema: Some(serde_json::json!({"type":"object"})),
                    },
                )?;
                assert_eq!(
                    response,
                    McpElicitationResponse::accept(serde_json::json!({"token":"abc"}))
                );
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": "accepted"}],
                    "isError": false
                }))
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resources": []}))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resourceTemplates": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Ok(serde_json::json!({"contents": []}))
            }
        }

        struct AcceptingHandler;

        impl McpElicitationHandler for AcceptingHandler {
            fn handle_elicitation(
                &self,
                request: McpElicitationRequest,
            ) -> Result<McpElicitationResponse, String> {
                assert_eq!(request.message, "Enter token");
                Ok(McpElicitationResponse::accept(
                    serde_json::json!({"token":"abc"}),
                ))
            }
        }

        let tool = McpTool {
            server: "prompts".to_string(),
            name: "authorize".to_string(),
            schema_name: "mcp__prompts__authorize".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        };
        let registry = McpRegistry::from_inner(McpRegistryInner {
            clients: HashMap::from([(
                "prompts".to_string(),
                Arc::new(McpClient {
                    config: McpServerConfig {
                        name: "prompts".to_string(),
                        ..Default::default()
                    },
                    server_name: "prompts".to_string(),
                    capabilities: McpServerCapabilities::default(),
                    transport: Mutex::new(Box::new(ElicitingTransport)),
                }),
            )]),
            tools: vec![tool.clone()],
            lookup: HashMap::from([(
                tool.schema_name.clone(),
                McpToolRef {
                    server: tool.server,
                    tool: tool.name,
                    schema_name: tool.schema_name,
                },
            )]),
            ..Default::default()
        });
        let tool_ref = registry
            .resolve_tool("mcp__prompts__authorize")
            .expect("tool ref");

        let result = registry
            .call_tool_with_elicitation_handler(
                &tool_ref,
                serde_json::json!({}),
                Some(&AcceptingHandler),
            )
            .expect("tool result");

        assert_eq!(result.output, "accepted");
    }

    #[test]
    fn registry_aggregates_mcp_resource_list_errors_without_losing_successes() {
        struct ResourceListTransport {
            result: Result<Value, String>,
        }

        impl McpTransport for ResourceListTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("not used".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                self.result.clone()
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resourceTemplates": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Err("not used".to_string())
            }
        }

        let registry = McpRegistry::from_resource_transports_for_test([
            (
                "notes".to_string(),
                Box::new(ResourceListTransport {
                    result: Ok(serde_json::json!({
                        "resources": [
                            {
                                "uri": "memo://orca/one",
                                "name": "memo one",
                                "description": "A test memo",
                                "mimeType": "text/plain"
                            }
                        ]
                    })),
                }) as Box<dyn McpTransport>,
            ),
            (
                "broken".to_string(),
                Box::new(ResourceListTransport {
                    result: Err("resources/list timed out".to_string()),
                }) as Box<dyn McpTransport>,
            ),
        ]);

        let listing = registry.list_resources_with_errors(None);

        assert_eq!(listing.resources.len(), 1);
        assert_eq!(listing.resources[0].server, "notes");
        assert_eq!(listing.resources[0].uri, "memo://orca/one");
        assert_eq!(
            listing.errors,
            vec!["broken: resources/list timed out".to_string()]
        );

        let single_server_error = registry
            .list_resources(Some("broken"))
            .expect_err("single-server resource list should stay strict");
        assert_eq!(single_server_error, "resources/list timed out");
    }

    #[test]
    fn registry_includes_initialization_errors_in_all_server_resource_listing() {
        struct ResourceListTransport;

        impl McpTransport for ResourceListTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("not used".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({
                    "resources": [
                        {
                            "uri": "memo://orca/one",
                            "name": "memo one",
                            "description": "A test memo",
                            "mimeType": "text/plain"
                        }
                    ]
                }))
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resourceTemplates": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Err("not used".to_string())
            }
        }

        let registry = McpRegistry::from_resource_transports_for_test([(
            "notes".to_string(),
            Box::new(ResourceListTransport) as Box<dyn McpTransport>,
        )])
        .with_registry_errors_for_test(vec![
            "failed to start MCP server 'broken': boom".to_string(),
        ]);

        let listing = registry.list_resources_with_errors(None);

        assert_eq!(listing.resources.len(), 1);
        assert_eq!(listing.resources[0].server, "notes");
        assert_eq!(
            listing.errors,
            vec!["failed to start MCP server 'broken': boom".to_string()]
        );

        let single_server_listing = registry
            .list_resources(Some("notes"))
            .expect("single-server resource listing");
        assert_eq!(single_server_listing.len(), 1);
    }

    #[test]
    fn registry_aggregates_mcp_resource_template_errors_without_losing_successes() {
        struct TemplateListTransport {
            result: Result<Value, String>,
        }

        impl McpTransport for TemplateListTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("not used".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resources": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Err("not used".to_string())
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                self.result.clone()
            }
        }

        let registry = McpRegistry::from_resource_transports_for_test([
            (
                "docs".to_string(),
                Box::new(TemplateListTransport {
                    result: Ok(serde_json::json!({
                        "resourceTemplates": [
                            {
                                "uriTemplate": "file:///{path}",
                                "name": "workspace file",
                                "description": "A file exposed by path",
                                "mimeType": "text/plain"
                            }
                        ]
                    })),
                }) as Box<dyn McpTransport>,
            ),
            (
                "broken".to_string(),
                Box::new(TemplateListTransport {
                    result: Err("resources/templates/list timed out".to_string()),
                }) as Box<dyn McpTransport>,
            ),
        ]);

        let listing = registry.list_resource_templates_with_errors(None);

        assert_eq!(listing.resource_templates.len(), 1);
        assert_eq!(listing.resource_templates[0].server, "docs");
        assert_eq!(listing.resource_templates[0].uri_template, "file:///{path}");
        assert_eq!(
            listing.errors,
            vec!["broken: resources/templates/list timed out".to_string()]
        );

        let single_server_error = registry
            .list_resource_templates(Some("broken"))
            .expect_err("single-server resource template list should stay strict");
        assert_eq!(single_server_error, "resources/templates/list timed out");
    }

    #[test]
    fn registry_includes_initialization_errors_in_all_server_resource_template_listing() {
        struct TemplateListTransport;

        impl McpTransport for TemplateListTransport {
            fn initialize(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("not used".to_string())
            }

            fn list_resources(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"resources": []}))
            }

            fn read_resource(&self, _uri: &str) -> Result<Value, String> {
                Err("not used".to_string())
            }

            fn list_resource_templates(&self) -> Result<Value, String> {
                Ok(serde_json::json!({
                    "resourceTemplates": [
                        {
                            "uriTemplate": "file:///{path}",
                            "name": "workspace file",
                            "description": "A file exposed by path",
                            "mimeType": "text/plain"
                        }
                    ]
                }))
            }
        }

        let registry = McpRegistry::from_resource_transports_for_test([(
            "docs".to_string(),
            Box::new(TemplateListTransport) as Box<dyn McpTransport>,
        )])
        .with_registry_errors_for_test(vec![
            "failed to start MCP server 'broken': boom".to_string(),
        ]);

        let listing = registry.list_resource_templates_with_errors(None);

        assert_eq!(listing.resource_templates.len(), 1);
        assert_eq!(listing.resource_templates[0].server, "docs");
        assert_eq!(
            listing.errors,
            vec!["failed to start MCP server 'broken': boom".to_string()]
        );

        let single_server_listing = registry
            .list_resource_templates(Some("docs"))
            .expect("single-server resource template listing");
        assert_eq!(single_server_listing.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn all_server_resource_listing_skips_servers_without_resource_capability() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let resources_server = temp_dir.path().join("resources_server.sh");
        std::fs::write(
            &resources_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://orca/one","name":"memo one","description":"A test memo","mimeType":"text/plain"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write resource MCP fixture");
        let tools_only_server = temp_dir.path().join("tools_only_server.sh");
        std::fs::write(
            &tools_only_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"tools-only","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"resources/list unsupported"}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write tools-only MCP fixture");
        for server in [&resources_server, &tools_only_server] {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(server, permissions).expect("chmod MCP fixture");
        }

        let registry = initialize_registry(&[
            stdio_fixture_config("resources", &resources_server),
            stdio_fixture_config("tools_only", &tools_only_server),
        ]);

        let listing = registry.list_resources_with_errors(None);

        assert_eq!(listing.resources.len(), 1);
        assert_eq!(listing.resources[0].server, "resources");
        assert_eq!(listing.resources[0].uri, "memo://orca/one");
        assert!(
            listing.errors.is_empty(),
            "tools-only server should be skipped, got {:?}",
            listing.errors
        );

        let explicit_error = registry
            .list_resources(Some("tools_only"))
            .expect_err("explicit server filter should still call the selected server");
        assert!(explicit_error.contains("resources/list unsupported"));
    }

    #[cfg(unix)]
    #[test]
    fn all_server_resource_template_listing_skips_servers_without_resource_capability() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let resources_server = temp_dir.path().join("resource_templates_server.sh");
        std::fs::write(
            &resources_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[{"uriTemplate":"file:///{path}","name":"workspace file","description":"A file exposed by path","mimeType":"text/plain"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write resource templates MCP fixture");
        let tools_only_server = temp_dir.path().join("tools_only_templates_server.sh");
        std::fs::write(
            &tools_only_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"tools-only","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"resources/templates/list unsupported"}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write tools-only MCP fixture");
        for server in [&resources_server, &tools_only_server] {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(server, permissions).expect("chmod MCP fixture");
        }

        let registry = initialize_registry(&[
            stdio_fixture_config("resources", &resources_server),
            stdio_fixture_config("tools_only", &tools_only_server),
        ]);

        let listing = registry.list_resource_templates_with_errors(None);

        assert_eq!(listing.resource_templates.len(), 1);
        assert_eq!(listing.resource_templates[0].server, "resources");
        assert_eq!(listing.resource_templates[0].uri_template, "file:///{path}");
        assert!(
            listing.errors.is_empty(),
            "tools-only server should be skipped, got {:?}",
            listing.errors
        );

        let explicit_error = registry
            .list_resource_templates(Some("tools_only"))
            .expect_err("explicit server filter should still call the selected server");
        assert!(explicit_error.contains("resources/templates/list unsupported"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_lists_and_reads_mcp_resources_from_stdio_server() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("resource_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://orca/one","name":"memo one","description":"A test memo","mimeType":"text/plain"}]}}\n'
      ;;
    *'"method":"resources/read"'*)
      printf '{"jsonrpc":"2.0","id":4,"result":{"contents":[{"uri":"memo://orca/one","mimeType":"text/plain","text":"resource body"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }

        let registry = initialize_registry(&[stdio_fixture_config("resources", &server)]);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );

        let resources = registry
            .list_resources(None)
            .expect("list all MCP resources");
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].server, "resources");
        assert_eq!(resources[0].uri, "memo://orca/one");
        assert_eq!(resources[0].name, "memo one");
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));

        let content = registry
            .read_resource("resources", "memo://orca/one")
            .expect("read MCP resource");
        assert_eq!(content.contents.len(), 1);
        assert_eq!(content.contents[0].text.as_deref(), Some("resource body"));
        assert_eq!(content.contents[0].mime_type.as_deref(), Some("text/plain"));
    }

    #[cfg(unix)]
    #[test]
    fn registry_lists_mcp_resource_templates_from_stdio_server() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("resource_template_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"templates","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[{"uriTemplate":"file:///{path}","name":"workspace file","description":"A file exposed by path","mimeType":"text/plain"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }

        let registry = initialize_registry(&[stdio_fixture_config("templates", &server)]);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );

        let templates = registry
            .list_resource_templates(None)
            .expect("list all MCP resource templates");
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].server, "templates");
        assert_eq!(templates[0].uri_template, "file:///{path}");
        assert_eq!(templates[0].name, "workspace file");
        assert_eq!(templates[0].mime_type.as_deref(), Some("text/plain"));
    }

    #[cfg(unix)]
    #[test]
    fn stdio_client_reconnects_after_timed_out_tool_call() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let state_dir = temp_dir.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let server = temp_dir.path().join("reconnecting_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
state_dir="$1"
run_file="$state_dir/run-count"
call_file="$state_dir/call-count"
run_count=0
if [ -f "$run_file" ]; then
  run_count=$(cat "$run_file")
fi
run_count=$((run_count + 1))
printf '%s' "$run_count" > "$run_file"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      call_count=0
      if [ -f "$call_file" ]; then
        call_count=$(cat "$call_file")
      fi
      call_count=$((call_count + 1))
      printf '%s' "$call_count" > "$call_file"
      if [ "$call_count" -eq 1 ]; then
        sleep 5
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"reconnected"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let registry = initialize_registry(&[McpServerConfig {
            name: "slow".to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![
                server.to_string_lossy().into_owned(),
                state_dir.to_string_lossy().into_owned(),
            ],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(100),
            ..Default::default()
        }]);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );
        let tool_ref = registry
            .resolve_tool("mcp__slow__wait")
            .expect("registered MCP tool");

        let first = registry.call_tool(&tool_ref, serde_json::json!({}));
        assert!(
            first
                .unwrap_err()
                .contains("MCP request 'tools/call' timed out after 100ms")
        );

        let second = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("second call should reconnect");

        assert_eq!(second.output, "reconnected");
        let runs = std::fs::read_to_string(state_dir.join("run-count")).expect("run count");
        assert_eq!(runs, "2");
    }

    /// Builds a valid RGBA PNG whose pixel rows are stored uncompressed
    /// (deflate "stored" blocks), so its size tracks `width * height` the way
    /// a detailed screenshot's does.
    #[cfg(unix)]
    fn uncompressed_png(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let table = (0..256u32)
                .map(|entry| {
                    (0..8).fold(entry, |crc, _| {
                        if crc & 1 == 1 {
                            0xEDB8_8320 ^ (crc >> 1)
                        } else {
                            crc >> 1
                        }
                    })
                })
                .collect::<Vec<_>>();
            !bytes.iter().fold(!0u32, |crc, byte| {
                table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8)
            })
        }
        fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            let length = u32::try_from(data.len()).expect("PNG chunk length");
            png.extend_from_slice(&length.to_be_bytes());
            let start = png.len();
            png.extend_from_slice(kind);
            png.extend_from_slice(data);
            let crc = crc32(&png[start..]);
            png.extend_from_slice(&crc.to_be_bytes());
        }

        let mut rows = Vec::new();
        for y in 0..height {
            rows.push(0); // filter type: none
            for x in 0..width {
                rows.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 0xFF]);
            }
        }
        let mut zlib = vec![0x78, 0x01];
        let blocks = rows.chunks(usize::from(u16::MAX)).collect::<Vec<_>>();
        for (index, block) in blocks.iter().enumerate() {
            // BFINAL on the last block; BTYPE 00 means stored.
            zlib.push(u8::from(index + 1 == blocks.len()));
            let length = u16::try_from(block.len()).expect("stored block length");
            zlib.extend_from_slice(&length.to_le_bytes());
            zlib.extend_from_slice(&(!length).to_le_bytes());
            zlib.extend_from_slice(block);
        }
        let (a, b) = rows.iter().fold((1u32, 0u32), |(a, b), byte| {
            let a = (a + u32::from(*byte)) % 65_521;
            (a, (b + a) % 65_521)
        });
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        // 8-bit RGBA, deflate, adaptive filtering, no interlace.
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        push_chunk(&mut png, b"IHDR", &header);
        push_chunk(&mut png, b"IDAT", &zlib);
        push_chunk(&mut png, b"IEND", &[]);
        png
    }

    #[cfg(unix)]
    #[test]
    fn a_screenshot_sized_png_arrives_through_the_stdio_transport() {
        use base64::Engine as _;

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("screenshot_mcp_server.sh");
        let response_file = temp_dir.path().join("screenshot-response.jsonl");
        // 1024x768 RGBA stored uncompressed: about 3 MiB, the size of a
        // detailed Retina screenshot.
        let png = uncompressed_png(1024, 768);
        assert!(png.len() > 3 * 1024 * 1024, "PNG is {} bytes", png.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&png);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {
                "content": [
                    {"type": "text", "text": "screenshot taken"},
                    {"type": "image", "data": &data, "mimeType": "image/png"}
                ],
                "isError": false
            }
        });
        std::fs::write(&response_file, format!("{response}\n")).expect("write MCP response");
        std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"screen","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"capture","description":"captures the screen","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      cat "$1"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let mut config = stdio_fixture_config("screen", &server);
        config
            .args
            .push(response_file.to_string_lossy().into_owned());
        config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
        let registry = initialize_registry(&[config]);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let tool_ref = registry
            .resolve_tool("mcp__screen__capture")
            .expect("screen capture tool ref");

        let result = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("a screenshot-sized image fits in one MCP response");

        assert_eq!(result.output, "screenshot taken\n[1 image attached]");
        assert_eq!(result.images.len(), 1);
        assert!(matches!(
            &result.images[0].source,
            orca_core::conversation::ImageSource::Base64 { media_type, data: sent }
                if media_type == "image/png" && *sent == data
        ));
    }

    #[cfg(unix)]
    #[test]
    fn stdio_client_reconnects_after_an_oversized_tool_response() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let state_dir = temp_dir.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let oversized = temp_dir.path().join("oversized-response.jsonl");
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {
                "content": [{
                    "type": "text",
                    "text": "x".repeat(crate::transport::MAX_STDIO_RESPONSE_LINE_BYTES)
                }],
                "isError": false
            }
        });
        std::fs::write(&oversized, format!("{response}\n")).expect("write oversized response");
        let server = temp_dir.path().join("oversized_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
state_dir="$1"
run_file="$state_dir/run-count"
run_count=0
if [ -f "$run_file" ]; then
  run_count=$(cat "$run_file")
fi
run_count=$((run_count + 1))
printf '%s' "$run_count" > "$run_file"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"oversized","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"capture","description":"captures the screen","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      if [ "$run_count" -eq 1 ]; then
        cat "$2"
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"reconnected"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let mut config = stdio_fixture_config("oversized", &server);
        config.args.extend([
            state_dir.to_string_lossy().into_owned(),
            oversized.to_string_lossy().into_owned(),
        ]);
        config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
        let registry = initialize_registry(&[config]);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let tool_ref = registry
            .resolve_tool("mcp__oversized__capture")
            .expect("registered MCP tool");

        let first = registry.call_tool(&tool_ref, serde_json::json!({}));
        let second = registry.call_tool(&tool_ref, serde_json::json!({}));

        let first = first.expect_err("an oversized response fails its call");
        assert!(
            first.contains("MCP response exceeded maximum line size"),
            "unexpected oversized response error: {first}"
        );
        assert_eq!(
            second
                .expect("the next call must reach a reconnected server")
                .output,
            "reconnected"
        );
        let runs = std::fs::read_to_string(state_dir.join("run-count")).expect("run count");
        assert_eq!(runs, "2");
    }

    /// A stdio server that lists the tools in `<dir>/<name>/tools.json` and
    /// adds a line to `<dir>/<name>/starts` each time it starts.
    #[cfg(unix)]
    fn listing_server_config(name: &str, dir: &std::path::Path, tools: &str) -> McpServerConfig {
        let state = dir.join(name);
        std::fs::create_dir_all(&state).expect("state dir");
        std::fs::write(state.join("tools.json"), tools).expect("write the tool list");
        let script = dir.join(format!("{name}.sh"));
        std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
printf 'start\n' >> "$state_dir/starts"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"listing","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":%s}}\n' "$(cat "$state_dir/tools.json")"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let mut config = stdio_fixture_config(name, &script);
        config.args.push(state.to_string_lossy().into_owned());
        config
    }

    #[cfg(unix)]
    fn starts(dir: &std::path::Path, name: &str) -> usize {
        std::fs::read_to_string(dir.join(name).join("starts"))
            .expect("read the start log")
            .lines()
            .count()
    }

    fn schema_names(registry: &McpRegistry) -> Vec<String> {
        registry
            .tools()
            .into_iter()
            .map(|tool| tool.schema_name)
            .collect()
    }

    /// A stdio server whose command does not exist.
    fn missing_server_config(name: &str, dir: &std::path::Path) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some(dir.join("missing-server").to_string_lossy().into_owned()),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn reconnecting_a_server_replaces_only_its_tools() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let first = listing_server_config(
            "first",
            temp_dir.path(),
            r#"[{"name":"old_tool","inputSchema":{"type":"object"}}]"#,
        );
        let second = listing_server_config(
            "second",
            temp_dir.path(),
            r#"[{"name":"stable","inputSchema":{"type":"object"}}]"#,
        );
        let registry = initialize_registry(&[first, second]);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        assert_eq!(
            schema_names(&registry),
            ["mcp__first__old_tool", "mcp__second__stable"]
        );
        // A tool registry holds a clone; it must see the new tools too.
        let held = registry.clone();
        std::fs::write(
            temp_dir.path().join("first").join("tools.json"),
            r#"[{"name":"new_tool","inputSchema":{"type":"object"}}]"#,
        )
        .expect("change the first server's tools");

        registry
            .reconnect_server("first")
            .expect("reconnect the first server");

        assert_eq!(
            schema_names(&held),
            ["mcp__first__new_tool", "mcp__second__stable"]
        );
        assert!(held.resolve_tool("mcp__first__old_tool").is_none());
        assert!(held.resolve_tool("mcp__first__new_tool").is_some());
        assert!(held.resolve_tool("mcp__second__stable").is_some());
        assert_eq!(starts(temp_dir.path(), "first"), 2);
        assert_eq!(
            starts(temp_dir.path(), "second"),
            1,
            "the second server keeps its connection"
        );
        assert_eq!(
            held.server_states(),
            [
                ("first".to_string(), McpServerState::Ready),
                ("second".to_string(), McpServerState::Ready),
            ]
        );
    }

    #[test]
    fn a_failed_server_is_listed_with_its_error() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let registry = initialize_registry(&[missing_server_config("broken", temp_dir.path())]);

        let errors = registry.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("failed to start MCP server 'broken'"),
            "{errors:?}"
        );
        assert_eq!(
            registry.server_states(),
            [(
                "broken".to_string(),
                McpServerState::Failed {
                    message: errors[0].clone()
                }
            )]
        );

        let error = registry
            .reconnect_server("broken")
            .expect_err("the server's command is still missing");
        assert!(
            error.starts_with("failed to start MCP server 'broken'"),
            "{error}"
        );
        assert_eq!(
            registry.server_states(),
            [(
                "broken".to_string(),
                McpServerState::Failed { message: error }
            )]
        );
        assert!(registry.tools().is_empty());
    }

    #[test]
    fn a_server_that_needs_login_reports_needs_login() {
        use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};
        use orca_core::config::mcp_credentials::{McpCredential, save_mcp_credential};

        let server = OAuthTestServer::start(OAuthTestBehavior {
            accepted_tokens: vec!["at-stored".to_string()],
            ..Default::default()
        });
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        let registry = initialize_registry_with_credentials(
            &[server.config("docs")],
            Some(credentials.clone()),
        );

        assert_eq!(
            registry.server_states(),
            [("docs".to_string(), McpServerState::NeedsLogin)]
        );
        assert_eq!(
            registry.errors(),
            ["MCP server requires login: run 'orca mcp login docs'"]
        );
        assert!(registry.tools().is_empty());

        // After `orca mcp login`, a reconnect sends the stored token.
        save_mcp_credential(
            &credentials,
            "docs",
            &McpCredential {
                server_url: server.mcp_url(),
                access_token: "at-stored".to_string(),
                refresh_token: None,
                expires_at: None,
                token_endpoint: format!("{}/token", server.url()),
                client_id: "configured-client".to_string(),
                resource: server.mcp_url(),
                scope: None,
            },
        )
        .expect("store a login");
        registry
            .reconnect_server("docs")
            .expect("reconnect after logging in");

        assert_eq!(
            registry.server_states(),
            [("docs".to_string(), McpServerState::Ready)]
        );
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        assert_eq!(schema_names(&registry), ["mcp__docs__echo"]);
    }

    #[test]
    fn disabled_servers_are_listed() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut disabled = missing_server_config("off", temp_dir.path());
        disabled.disabled = true;
        let registry =
            initialize_registry(&[disabled, missing_server_config("broken", temp_dir.path())]);

        let states = registry.server_states();
        assert_eq!(states.len(), 2, "{states:?}");
        assert_eq!(states[0], ("off".to_string(), McpServerState::Disabled));
        assert_eq!(states[1].0, "broken");
        assert!(
            matches!(states[1].1, McpServerState::Failed { .. }),
            "{states:?}"
        );
        assert_eq!(
            registry.errors().len(),
            1,
            "a disabled server is never started: {:?}",
            registry.errors()
        );

        assert_eq!(
            registry.reconnect_server("off"),
            Err("MCP server 'off' is disabled".to_string())
        );
        assert_eq!(
            registry.server_states()[0],
            ("off".to_string(), McpServerState::Disabled)
        );
        assert_eq!(
            registry.reconnect_server("nope"),
            Err("no MCP server named 'nope'".to_string())
        );
    }
}
