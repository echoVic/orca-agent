use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};
use std::time::Duration;

use serde_json::Value;

use crate::auth::is_auth_required;
use crate::client::{McpClient, McpRequestError, McpServerCapabilities};
use crate::connection_stop::McpConnectionStop;
use crate::transport::{self, McpElicitationHandler, McpTransport};
use orca_core::conversation::ImageInput;
use orca_core::mcp_types::{
    CallToolResult, GetPromptResult, McpContent, McpPrompt, McpPromptContent, McpPromptDescriptor,
    McpResource, McpResourceTemplate, McpServerConfig, McpTool, McpToolRef, McpTransportKind,
    PromptsListResult, ReadResourceResult, ResourceTemplatesListResult, ResourcesListResult,
    ToolsListResult, canonical_mcp_name, tool_is_enabled,
};
use orca_core::tool_images::tool_image;

/// The MCP servers of a session and their tools. Clones share one registry,
/// so a server reconnected through any clone serves every holder's next
/// call. Each server connects in the background, on a thread of its own;
/// [`McpRegistry::subscribe`] hears of each change.
///
/// Its servers stop once the last clone is dropped, those still connecting
/// too, or as soon as one holder closes it ([`McpRegistry::close`]).
#[derive(Clone, Default)]
pub struct McpRegistry {
    shared: Arc<McpRegistryShared>,
}

/// What every clone of a registry shares.
#[derive(Default)]
struct McpRegistryShared {
    inner: RwLock<McpRegistryInner>,
    /// Who hears of each change. It has a lock of its own, so that a
    /// subscriber may read the registry while it is told.
    subscribers: Mutex<McpSubscribers>,
}

impl McpRegistryShared {
    fn subscribers(&self) -> MutexGuard<'_, McpSubscribers> {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for McpRegistryShared {
    fn drop(&mut self) {
        // The last clone is gone, so no one can use a server any more. A
        // connection still being made is stopped now: its thread holds the
        // registry only weakly, and the process may be about to exit, which
        // would end the thread and leave its server running.
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        for server in &inner.servers {
            server.stop.stop();
        }
    }
}

type McpChangeCallback = Arc<dyn Fn(&McpRegistry) + Send + Sync>;

#[derive(Default)]
struct McpSubscribers {
    next_id: u64,
    callbacks: Vec<(u64, McpChangeCallback)>,
}

/// Calls made by [`McpRegistry::subscribe`]. Dropping it ends them.
#[must_use = "dropping the subscription ends it"]
pub struct McpChangeSubscription {
    registry: Weak<McpRegistryShared>,
    id: u64,
}

impl Drop for McpChangeSubscription {
    fn drop(&mut self) {
        if let Some(shared) = self.registry.upgrade() {
            shared
                .subscribers()
                .callbacks
                .retain(|(id, _)| *id != self.id);
        }
    }
}

/// How a configured MCP server stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpServerState {
    /// A connection is being made, and it has no client meanwhile: its
    /// first, or that of a stdio server being reconnected, which is stopped
    /// first.
    Starting,
    /// Connected, with its tools registered.
    Ready,
    /// The last attempt to connect failed.
    Failed { message: String },
    /// The server wants the user to log in (`orca mcp login`).
    NeedsLogin,
    /// Turned off in the config, so never connected.
    Disabled,
}

/// A configured MCP server and how it stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerStatus {
    /// The canonical name, as in its tools' names.
    pub name: String,
    pub state: McpServerState,
    /// Why its prompts could not be listed, when it connected but
    /// `prompts/list` failed.
    pub prompts_error: Option<String>,
    /// What went wrong when it was last connected, as
    /// [`McpRegistry::errors`] lists it: why it failed to connect or needs a
    /// login, or the tools and prompts it offered that were left out; and
    /// since then, the resource lists it gave only part of.
    pub errors: Vec<String>,
}

/// How a registry stands at one moment ([`McpRegistry::snapshot`]): every
/// configured server and how it stands, and the tools and prompts of the
/// connected ones, all read together.
#[derive(Clone, Debug, Default)]
pub struct McpRegistrySnapshot {
    /// As [`McpRegistry::server_statuses`] lists them.
    pub servers: Vec<McpServerStatus>,
    /// As [`McpRegistry::tools`] lists them.
    pub tools: Vec<McpTool>,
    /// As [`McpRegistry::prompts`] lists them.
    pub prompts: Vec<McpPrompt>,
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
    /// Whether the registry was closed ([`McpRegistry::close`]): its servers
    /// are stopped for good, and no connection is put in place any more.
    closed: bool,
}

#[derive(Clone)]
struct McpServerEntry {
    /// The canonical name, as in its tools' names.
    name: String,
    config: McpServerConfig,
    state: McpServerState,
    /// The connection attempt the server takes its state from: 1 for the
    /// first, one more for each reconnect. An attempt whose number is no
    /// longer this one was overtaken, and is dropped when it is done.
    generation: u64,
    /// Stops the server of that attempt, and of the client it became.
    stop: Arc<McpConnectionStop>,
    /// Its registered tools.
    tools: Vec<McpTool>,
    /// The prompts it offers.
    prompts: Vec<McpPrompt>,
    /// Why its prompts could not be listed, when `prompts/list` failed.
    prompts_error: Option<String>,
    /// What went wrong when it was last connected: the failure, or tools
    /// and prompts left out because their names were taken or missing, or
    /// lists it gave only part of; and since then, the resource lists it
    /// gave only part of.
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

    /// Every configured server and how it stands, in config order.
    fn server_statuses(&self) -> Vec<McpServerStatus> {
        self.servers
            .iter()
            .map(|server| McpServerStatus {
                name: server.name.clone(),
                state: server.state.clone(),
                prompts_error: server.prompts_error.clone(),
                errors: server.errors.clone(),
            })
            .collect()
    }

    /// The prompts of every connected server, in config order.
    fn prompts(&self) -> Vec<McpPrompt> {
        self.servers
            .iter()
            .flat_map(|server| server.prompts.iter().cloned())
            .collect()
    }

    /// The index of `server_name`, while connection attempt `generation` is
    /// still the one it takes its state from, and the registry is open.
    fn current_attempt(&self, server_name: &str, generation: u64) -> Option<usize> {
        if self.closed {
            return None;
        }
        self.servers
            .iter()
            .position(|server| server.name == server_name && server.generation == generation)
    }

    /// Puts what a connection attempt came to in place of what the server
    /// at `index` had: its client, tools and prompts, or, when it failed,
    /// none and why. Returns the client it replaced, which stops its server
    /// when dropped; drop it once the lock is released.
    fn apply_connection(
        &mut self,
        index: usize,
        connected: Result<ConnectedServer, String>,
    ) -> Option<Arc<McpClient>> {
        let server_name = self.servers[index].name.clone();
        let mut taken = self
            .servers
            .iter()
            .filter(|server| server.name != server_name)
            .flat_map(|server| server.tools.iter().map(|tool| tool.schema_name.clone()))
            .collect::<HashSet<_>>();
        let server = &mut self.servers[index];
        server.errors.clear();
        let replaced = match connected {
            Ok(connected) => {
                server.tools = take_tool_names(
                    &server_name,
                    connected.tools,
                    &mut taken,
                    &mut server.errors,
                );
                server.prompts = connected.prompts;
                server.prompts_error = connected.prompts_error;
                server.errors.extend(connected.warnings);
                server.state = McpServerState::Ready;
                self.clients.insert(server_name, Arc::new(connected.client))
            }
            Err(error) => {
                server.state = failed_state(&error);
                server.tools.clear();
                server.prompts.clear();
                server.prompts_error = None;
                server.errors.push(error);
                self.clients.remove(&server_name)
            }
        };
        self.index_tools();
        replaced
    }
}

/// A prompt as its server expanded it (`prompts/get`): the text and the
/// images of its messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpPromptExpansion {
    pub text: String,
    pub images: Vec<ImageInput>,
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

/// The registry of the servers in `configs`, returned at once. Each enabled
/// server starts as [`McpServerState::Starting`] and connects on a thread of
/// its own, `orca-mcp-connect-<name>`, reading a stored OAuth login from
/// `credentials_path`; subscribers hear as each one connects or fails.
/// [`McpRegistry::wait_for_startup`] waits for all of them.
pub fn initialize_registry(
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
        let server_name = canonical_mcp_name(&config.name);
        if server_name.is_empty() {
            continue;
        }
        let owner = owners.entry(server_name).or_insert(index);
        if configs[*owner].disabled && !config.disabled {
            *owner = index;
        }
    }
    let mut starting = Vec::new();
    for (index, config) in configs.iter().enumerate() {
        let server_name = canonical_mcp_name(&config.name);
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
        let stop = Arc::new(McpConnectionStop::default());
        let (state, generation) = if config.disabled {
            (McpServerState::Disabled, 0)
        } else {
            starting.push((server_name.clone(), config.clone(), Arc::clone(&stop)));
            (McpServerState::Starting, FIRST_CONNECTION)
        };
        inner.servers.push(McpServerEntry {
            name: server_name,
            config: config.clone(),
            state,
            generation,
            stop,
            tools: Vec::new(),
            prompts: Vec::new(),
            prompts_error: None,
            errors: Vec::new(),
        });
    }
    let registry = McpRegistry::from_inner(inner);
    for (server_name, config, stop) in starting {
        registry.connect_in_background(server_name, config, stop);
    }
    registry
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

/// A server that answered, and what it offers.
struct ConnectedServer {
    client: McpClient,
    tools: Vec<McpTool>,
    prompts: Vec<McpPrompt>,
    /// Why its prompts could not be listed, when `prompts/list` failed.
    prompts_error: Option<String>,
    /// Problems that did not stop it connecting, such as prompts left out.
    warnings: Vec<String>,
}

/// Connects to the server `config` describes, under `stop`, which then
/// stops the server of the client this makes.
fn connect_server(
    config: &McpServerConfig,
    server_name: &str,
    credentials_path: Option<PathBuf>,
    stop: Arc<McpConnectionStop>,
) -> Result<ConnectedServer, String> {
    // An attempt must end, even in a panic: a server left starting would
    // keep `wait_for_startup` waiting for good.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let transport = stop.start(server_name, || {
            transport::connect_with_credentials(config, credentials_path.clone())
        })?;
        connect_server_with_transport(config, server_name, credentials_path, transport, stop)
    }))
    .unwrap_or_else(|_| Err(format!("connecting to MCP server '{server_name}' panicked")))
}

fn connect_server_with_transport(
    config: &McpServerConfig,
    server_name: &str,
    credentials_path: Option<PathBuf>,
    transport: Arc<dyn McpTransport>,
    stop: Arc<McpConnectionStop>,
) -> Result<ConnectedServer, String> {
    let initialize_result = transport.initialize()?;
    let capabilities = McpServerCapabilities::from_initialize_result(&initialize_result);
    let mut warnings = Vec::new();
    let (listed_tools, warning) = collect_pages(|cursor| {
        let list: ToolsListResult = serde_json::from_value(transport.list_tools(cursor)?)
            .map_err(|error| format!("invalid tools/list result for '{server_name}': {error}"))?;
        Ok((list.tools, list.next_cursor))
    })?;
    warnings.extend(warning.map(|warning| incomplete_list(server_name, "tools/list", &warning)));

    let tools = listed_tools
        .into_iter()
        .filter(|tool| tool_is_enabled(config, &tool.name))
        .map(|tool| {
            let tool_name = canonical_mcp_name(&tool.name);
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

    let client = McpClient {
        config: config.clone(),
        server_name: server_name.to_string(),
        credentials_path,
        capabilities,
        reconnect_failure: Mutex::default(),
        stop,
        transport: Mutex::new(transport),
    };
    // A server whose prompts cannot be listed still serves its tools: a
    // failure that stopped its transport has connected it again, when it
    // could.
    let (prompts, prompts_error) = if client.capabilities.prompts {
        let listed = collect_pages(|cursor| {
            let list: PromptsListResult = serde_json::from_value(client.list_prompts(cursor)?)
                .map_err(|error| {
                    format!("invalid prompts/list result for '{server_name}': {error}")
                })?;
            Ok((list.prompts, list.next_cursor))
        });
        match listed {
            Ok((listed, warning)) => {
                warnings.extend(
                    warning.map(|warning| incomplete_list(server_name, "prompts/list", &warning)),
                );
                (listed_prompts(server_name, listed, &mut warnings), None)
            }
            Err(error) => (Vec::new(), Some(error)),
        }
    } else {
        (Vec::new(), None)
    };

    // A failure that stopped the server, which could not then be started
    // again, leaves no server to serve the tools.
    if let Some(error) = client.take_reconnect_failure() {
        return Err(error);
    }

    Ok(ConnectedServer {
        client,
        tools,
        prompts,
        prompts_error,
        warnings,
    })
}

/// The most pages a list is read to: a server that has more is cut off.
const MAX_LIST_PAGES: usize = 100;

/// Reads a list a page at a time: `fetch` is asked for the page that starts
/// at a cursor, the first time for the first page, and answers with its
/// items and the cursor of the next page, if there is one. An empty cursor
/// ends the list too, as other MCP clients take it. Returns the items of
/// every page read, and a warning when the list was cut off: when the
/// server named a cursor it had named before, which would go round forever,
/// or once [`MAX_LIST_PAGES`] pages are read. A page that cannot be read
/// fails the list.
fn collect_pages<T>(
    mut fetch: impl FnMut(Option<&str>) -> Result<(Vec<T>, Option<String>), String>,
) -> Result<(Vec<T>, Option<String>), String> {
    let mut items = Vec::new();
    let mut cursors = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let (page, next) = fetch(cursor.as_deref())?;
        items.extend(page);
        let Some(next) = next.filter(|next| !next.is_empty()) else {
            return Ok((items, None));
        };
        if !cursors.insert(next.clone()) {
            return Ok((items, Some("MCP server repeated a list cursor".to_string())));
        }
        cursor = Some(next);
    }
    Ok((
        items,
        Some(format!("MCP list stopped after {MAX_LIST_PAGES} pages")),
    ))
}

/// What a server's errors say of a list it gave only part of: `warning`,
/// from [`collect_pages`], on its `method` result.
fn incomplete_list(server_name: &str, method: &str, warning: &str) -> String {
    format!("incomplete {method} result for '{server_name}': {warning}")
}

/// The prompts a server listed, under `server_name`. A prompt without a
/// name, or with an argument without one, cannot be asked for, so it is
/// left out, with a note in `errors`.
fn listed_prompts(
    server_name: &str,
    prompts: Vec<McpPromptDescriptor>,
    errors: &mut Vec<String>,
) -> Vec<McpPrompt> {
    prompts
        .into_iter()
        .filter(|prompt| {
            if prompt.name.trim().is_empty() {
                errors.push(format!(
                    "MCP prompt without a name, skipping from '{server_name}'"
                ));
                false
            } else if prompt
                .arguments
                .iter()
                .any(|argument| argument.name.trim().is_empty())
            {
                errors.push(format!(
                    "MCP prompt '{}' has an argument without a name, skipping from '{server_name}'",
                    prompt.name
                ));
                false
            } else {
                true
            }
        })
        .map(|prompt| McpPrompt {
            server: server_name.to_string(),
            name: prompt.name,
            description: prompt.description,
            arguments: prompt.arguments,
        })
        .collect()
}

/// Puts the messages of an expanded prompt together, in order: their text
/// joined with blank lines, and their images. An image that cannot be sent
/// leaves its note in the text instead, and an embedded resource adds its
/// text. Roles, and content of any other kind, are not read.
fn expand_prompt(result: GetPromptResult) -> McpPromptExpansion {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for message in result.messages {
        match message.content {
            McpPromptContent::Text { text } => texts.push(text),
            McpPromptContent::Image { data, mime_type } => match tool_image(&mime_type, data) {
                Ok(image) => images.push(image),
                Err(rejected) => texts.push(rejected.note()),
            },
            McpPromptContent::Resource { resource } => texts.extend(resource.text),
            McpPromptContent::Other => {}
        }
    }
    texts.retain(|text| !text.is_empty());
    McpPromptExpansion {
        text: texts.join("\n\n"),
        images,
    }
}

impl McpRegistry {
    fn from_inner(inner: McpRegistryInner) -> Self {
        Self {
            shared: Arc::new(McpRegistryShared {
                inner: RwLock::new(inner),
                subscribers: Mutex::default(),
            }),
        }
    }

    /// The registry as it stands. Never hold the guard across a request to a
    /// server, or while subscribers are told of a change.
    fn read(&self) -> RwLockReadGuard<'_, McpRegistryInner> {
        self.shared
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, McpRegistryInner> {
        self.shared
            .inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls `on_change` after each change to a server: its state, its
    /// tools or its prompts. It is called on the thread that made the
    /// change, with no lock held, so it may read the registry; it should
    /// return soon. It must not hold a clone of the registry, which would
    /// keep the registry, and its servers, alive. Dropping the subscription
    /// ends the calls, though one already under way may finish.
    pub fn subscribe(
        &self,
        on_change: Arc<dyn Fn(&McpRegistry) + Send + Sync>,
    ) -> McpChangeSubscription {
        let mut subscribers = self.shared.subscribers();
        let id = subscribers.next_id;
        subscribers.next_id += 1;
        subscribers.callbacks.push((id, on_change));
        McpChangeSubscription {
            registry: Arc::downgrade(&self.shared),
            id,
        }
    }

    /// Connects `server_name` with `config`, its first connection, under
    /// `stop`, on a thread of its own. The thread holds the registry
    /// weakly, so it keeps no server running: once every holder has dropped
    /// the registry, or one has closed it, `stop` stops the server at once,
    /// and the connection, should it still be made, is dropped.
    fn connect_in_background(
        &self,
        server_name: String,
        config: McpServerConfig,
        stop: Arc<McpConnectionStop>,
    ) {
        let credentials_path = self.read().credentials_path.clone();
        let registry = Arc::downgrade(&self.shared);
        let spawned = std::thread::Builder::new()
            .name(format!("orca-mcp-connect-{server_name}"))
            .spawn({
                let server_name = server_name.clone();
                move || {
                    let connected = connect_server(&config, &server_name, credentials_path, stop);
                    let Some(shared) = registry.upgrade() else {
                        // Dropping the connection stops the server.
                        return;
                    };
                    let _ = McpRegistry { shared }.finish_connecting(
                        &server_name,
                        FIRST_CONNECTION,
                        connected,
                    );
                }
            });
        if let Err(error) = spawned {
            let _ = self.finish_connecting(
                &server_name,
                FIRST_CONNECTION,
                Err(format!(
                    "failed to start connecting to MCP server '{server_name}': {error}"
                )),
            );
        }
    }

    /// Puts what connection attempt `generation` of `server_name` came to
    /// in place, unless a newer attempt has started since, or the registry
    /// was closed, and returns how it went. Subscribers hear of the change
    /// once the lock is released. The client it replaced, or the connection
    /// of an overtaken attempt, is dropped after that, which stops its
    /// server.
    fn finish_connecting(
        &self,
        server_name: &str,
        generation: u64,
        connected: Result<ConnectedServer, String>,
    ) -> Result<(), String> {
        let result = connected.as_ref().map(|_| ()).map_err(String::clone);
        let mut inner = self.write();
        let Some(index) = inner.current_attempt(server_name, generation) else {
            drop(inner);
            drop(connected);
            return result;
        };
        let replaced = inner.apply_connection(index, connected);
        drop(inner);
        self.notify_subscribers();
        drop(replaced);
        result
    }

    /// Tells each subscriber that the registry changed. No lock is held
    /// while it is told.
    fn notify_subscribers(&self) {
        let callbacks = self
            .shared
            .subscribers()
            .callbacks
            .iter()
            .map(|(_, on_change)| Arc::clone(on_change))
            .collect::<Vec<_>>();
        for on_change in callbacks {
            on_change(self);
        }
    }

    /// Whether a server is [`McpServerState::Starting`]: still making its
    /// first connection, or a stdio server being reconnected.
    pub fn is_starting(&self) -> bool {
        self.read()
            .servers
            .iter()
            .any(|server| server.state == McpServerState::Starting)
    }

    /// Waits until no server is [`McpServerState::Starting`], each having
    /// connected or failed, which each one's startup timeout bounds, and
    /// returns `true`. Returns `false` as soon as `should_cancel`, asked
    /// every 25 ms, says to stop waiting.
    pub fn wait_for_startup(&self, should_cancel: &dyn Fn() -> bool) -> bool {
        loop {
            if !self.is_starting() {
                return true;
            }
            if should_cancel() {
                return false;
            }
            std::thread::sleep(STARTUP_POLL);
        }
    }

    /// Stops every server for good, through every clone: each stdio server's
    /// process is ended at once, those still connecting or reconnecting
    /// too, and a connection made after this is dropped. From then on each
    /// server is [`McpServerState::Failed`], none has a client, tools or
    /// prompts, and none can be reconnected. Dropping the last clone stops
    /// the servers too; an owner that is done with the registry closes it,
    /// as a clone may outlive it on a thread that the process ends without
    /// waiting for. Subscribers are not told: the registry is done.
    pub fn close(&self) {
        let (stops, clients) = {
            let mut inner = self.write();
            if inner.closed {
                return;
            }
            inner.closed = true;
            for server in &mut inner.servers {
                if server.state != McpServerState::Disabled {
                    server.state = McpServerState::Failed {
                        message: format!("MCP server '{}' was stopped", server.name),
                    };
                }
                server.tools.clear();
                server.prompts.clear();
                server.prompts_error = None;
            }
            inner.index_tools();
            let stops = inner
                .servers
                .iter()
                .map(|server| Arc::clone(&server.stop))
                .collect::<Vec<_>>();
            (stops, std::mem::take(&mut inner.clients))
        };
        // Stopping a server waits for its process to end, so no lock is held.
        for stop in stops {
            stop.stop();
        }
        drop(clients);
    }

    /// Whether `other` is a clone of this registry.
    pub fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
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

            fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("static resource transport does not support tool calls".to_string())
            }

            fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
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

            fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
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
                    credentials_path: None,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    reconnect_failure: Mutex::default(),
                    stop: Arc::default(),
                    transport: Mutex::new(Arc::new(StaticResourceTransport {
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
                        credentials_path: None,
                        capabilities: McpServerCapabilities::resource_capable_for_test(),
                        reconnect_failure: Mutex::default(),
                        stop: Arc::default(),
                        transport: Mutex::new(Arc::from(transport)),
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

    /// The prompts of every connected server, in config order.
    pub fn prompts(&self) -> Vec<McpPrompt> {
        self.read().prompts()
    }

    /// How every server stands, with the tools and prompts of the connected
    /// ones, read at one moment: a server that connects or fails meanwhile
    /// shows in all three, or in none.
    pub fn snapshot(&self) -> McpRegistrySnapshot {
        let inner = self.read();
        McpRegistrySnapshot {
            servers: inner.server_statuses(),
            tools: inner.tools.clone(),
            prompts: inner.prompts(),
        }
    }

    /// Asks `server` (its canonical or configured name) to expand its prompt
    /// `prompt` with `arguments`, and puts the messages it answers with
    /// together: their text in order, separated by blank lines, and their
    /// images. An image that cannot be sent leaves its note in the text, and
    /// an embedded resource adds its text. A server that is not connected,
    /// or did not list the prompt, is not asked.
    pub fn get_prompt(
        &self,
        server: &str,
        prompt: &str,
        arguments: &BTreeMap<String, String>,
    ) -> Result<McpPromptExpansion, String> {
        let server_name = canonical_mcp_name(server);
        let client = {
            let inner = self.read();
            let entry = inner
                .servers
                .iter()
                .find(|entry| entry.name == server_name)
                .ok_or_else(|| format!("no MCP server named '{server}'"))?;
            let client = inner
                .clients
                .get(&server_name)
                .cloned()
                .ok_or_else(|| format!("MCP server '{server}' is not connected"))?;
            if !entry.prompts.iter().any(|offered| offered.name == prompt) {
                return Err(format!(
                    "MCP server '{server}' has no prompt named '{prompt}'"
                ));
            }
            client
        };
        // The request can take the server's whole timeout, so no lock is held.
        let result = client.get_prompt(prompt, serde_json::json!(arguments));
        self.note_answer(&server_name, &client, &result);
        let result: GetPromptResult = serde_json::from_value(result?)
            .map_err(|error| format!("invalid prompts/get result for '{server_name}': {error}"))?;
        Ok(expand_prompt(result))
    }

    /// What is wrong with the config itself, such as a server without a
    /// name: the first of [`Self::errors`].
    pub fn config_errors(&self) -> Vec<String> {
        self.read().errors.clone()
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
    /// still starting, servers that failed to connect, and disabled servers
    /// are listed too.
    pub fn server_statuses(&self) -> Vec<McpServerStatus> {
        self.read().server_statuses()
    }

    /// Connects `name` again with its saved config, and replaces its client
    /// and tools with the new ones. When that fails, the server has no
    /// client or tools until it is reconnected, and its state says why.
    /// Other servers are left as they are. A connection still being made
    /// for the server, its first one included, is overtaken: it is dropped
    /// once made. So is this one, should another reconnect start before it
    /// is done.
    ///
    /// A stdio server is stopped before it is started again, so that one
    /// that listens on a fixed port can start: its client is taken out at
    /// once, and a call still under way on it fails. The server has no
    /// client until the new one is in, and is [`McpServerState::Starting`]
    /// meanwhile, which subscribers hear. A connection still being made for
    /// it is stopped too. A remote server keeps serving through its client,
    /// and keeps its state, until the new one replaces it. A closed registry
    /// reconnects nothing.
    pub fn reconnect_server(&self, name: &str) -> Result<(), String> {
        let server_name = canonical_mcp_name(name);
        let (config, credentials_path, generation, stop, stopped, now_starting) = {
            let mut guard = self.write();
            let inner = &mut *guard;
            let server = inner
                .servers
                .iter_mut()
                .find(|server| server.name == server_name)
                .ok_or_else(|| format!("no MCP server named '{name}'"))?;
            if server.config.disabled {
                return Err(format!("MCP server '{name}' is disabled"));
            }
            if inner.closed {
                return Err(format!("MCP server '{name}' was stopped"));
            }
            server.generation += 1;
            let stop = Arc::new(McpConnectionStop::default());
            let overtaken = std::mem::replace(&mut server.stop, Arc::clone(&stop));
            let mut stopped = None;
            let mut now_starting = false;
            if server.config.transport == McpTransportKind::Stdio {
                stopped = Some((overtaken, inner.clients.remove(&server_name)));
                now_starting = server.state != McpServerState::Starting;
                server.state = McpServerState::Starting;
            }
            (
                server.config.clone(),
                inner.credentials_path.clone(),
                server.generation,
                stop,
                stopped,
                now_starting,
            )
        };
        if now_starting {
            self.notify_subscribers();
        }
        // Stopping a server, and connecting, which can take up to the
        // startup timeout, are done with no lock held.
        if let Some((overtaken, client)) = stopped {
            overtaken.stop();
            drop(client);
        }
        let connected = connect_server(&config, &server_name, credentials_path, stop);
        self.finish_connecting(&server_name, generation, connected)
    }

    /// Notes what the answer to a request sent to `server` (its canonical
    /// name) through `client` says of the server: one after which its stdio
    /// server could not be started again marks it as failed, one that finds
    /// the login gone marks it as needing a login, and any that goes through
    /// marks it ready again.
    fn note_answer<T, E: fmt::Display>(
        &self,
        server: &str,
        client: &Arc<McpClient>,
        answer: &Result<T, E>,
    ) {
        if let Some(error) = client.take_reconnect_failure() {
            self.fail_through(server, client, error);
            return;
        }
        match answer {
            Ok(_) => self.mark_ready_after_success(server, client),
            Err(error) if is_auth_required(&error.to_string()) => {
                self.mark_needs_login(server, client);
            }
            Err(_) => {}
        }
    }

    /// Marks `server` (its canonical name) as failed with `error`, once a
    /// reconnect of `client`, a stdio server's, could not start the server
    /// again, and then tells the subscribers. As after a failed connection,
    /// the server has no client, tools or prompts until it is reconnected,
    /// and its state says why. A client that a reconnect of the registry's
    /// has replaced since speaks for a connection that is gone, so its
    /// failure changes nothing.
    fn fail_through(&self, server: &str, client: &Arc<McpClient>, error: String) {
        let replaced = {
            let mut inner = self.write();
            let current = inner
                .clients
                .get(server)
                .is_some_and(|current| Arc::ptr_eq(current, client));
            let index = inner.servers.iter().position(|entry| entry.name == server);
            match index {
                Some(index) if current => inner.apply_connection(index, Err(error)),
                _ => return,
            }
        };
        self.notify_subscribers();
        drop(replaced);
    }

    /// Marks `server` (its canonical name) as needing a login, once a
    /// request sent through `client` finds that its login no longer works,
    /// and tells the subscribers. Its client and tools stay until it is
    /// reconnected, which a login does.
    fn mark_needs_login(&self, server: &str, client: &Arc<McpClient>) {
        self.change_state_through(
            server,
            client,
            McpServerState::Ready,
            McpServerState::NeedsLogin,
        );
    }

    /// Marks `server` (its canonical name) ready again, when it was marked
    /// as needing a login, once a request sent through `client` goes
    /// through: its login works again, as when the user has logged in
    /// elsewhere since. Tells the subscribers.
    fn mark_ready_after_success(&self, server: &str, client: &Arc<McpClient>) {
        // Most requests find the server ready, which takes no write lock.
        let needs_login = self
            .read()
            .servers
            .iter()
            .any(|entry| entry.name == server && entry.state == McpServerState::NeedsLogin);
        if needs_login {
            self.change_state_through(
                server,
                client,
                McpServerState::NeedsLogin,
                McpServerState::Ready,
            );
        }
    }

    /// Changes the state of `server` (its canonical name) from `from` to
    /// `to`, while `client` is still its client, and then tells the
    /// subscribers. A client that a reconnect has replaced since speaks for
    /// a connection that is gone, so what its requests find changes nothing.
    fn change_state_through(
        &self,
        server: &str,
        client: &Arc<McpClient>,
        from: McpServerState,
        to: McpServerState,
    ) {
        let changed = {
            let mut inner = self.write();
            let current = inner
                .clients
                .get(server)
                .is_some_and(|current| Arc::ptr_eq(current, client));
            match inner.servers.iter_mut().find(|entry| entry.name == server) {
                Some(entry) if current && entry.state == from => {
                    entry.state = to;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.notify_subscribers();
        }
    }

    /// Adds `note` to the errors of `server` (its canonical name), unless
    /// they hold it already, while `client` is still its client, and then
    /// tells the subscribers. A client that a reconnect has replaced since
    /// speaks for a connection that is gone, and so do its notes.
    fn note_server_error(&self, server: &str, client: &Arc<McpClient>, note: String) {
        let changed = {
            let mut inner = self.write();
            let current = inner
                .clients
                .get(server)
                .is_some_and(|current| Arc::ptr_eq(current, client));
            match inner.servers.iter_mut().find(|entry| entry.name == server) {
                Some(entry) if current && !entry.errors.contains(&note) => {
                    entry.errors.push(note);
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.notify_subscribers();
        }
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

            fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err("static resource listing transport does not support tool calls".to_string())
            }

            fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
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

            fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
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
                    credentials_path: None,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    reconnect_failure: Mutex::default(),
                    stop: Arc::default(),
                    transport: Mutex::new(Arc::new(StaticResourceListingTransport {
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
                    credentials_path: None,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    reconnect_failure: Mutex::default(),
                    stop: Arc::default(),
                    transport: Mutex::new(Arc::new(StaticResourceListingTransport {
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

            fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                Err(
                    "static resource template listing transport does not support tool calls"
                        .to_string(),
                )
            }

            fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
                Ok(serde_json::json!({ "resources": [] }))
            }

            fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
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
                    credentials_path: None,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    reconnect_failure: Mutex::default(),
                    stop: Arc::default(),
                    transport: Mutex::new(Arc::new(StaticResourceTemplateListingTransport {
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
                    credentials_path: None,
                    capabilities: McpServerCapabilities::resource_capable_for_test(),
                    reconnect_failure: Mutex::default(),
                    stop: Arc::default(),
                    transport: Mutex::new(Arc::new(StaticResourceTemplateListingTransport {
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
        );
        self.note_answer(&tool_ref.server, &client, &result);
        let result: CallToolResult = serde_json::from_value(result?)
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
            let (listed, warning) = self
                .listed_resources(&server, &client, should_cancel)
                .map_err(McpRequestError::from_message)?;
            // What the server gave of a list it cut short is kept, and noted
            // on the server, as the listing has no errors of its own.
            if let Some(warning) = warning {
                let note = incomplete_list(&server, "resources/list", &warning);
                self.note_server_error(&server, &client, note);
            }
            resources.extend(listed);
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

    /// The resources of `server`, or of every server that has resources,
    /// with an error for each server that could not be listed, and for each
    /// list a server cut short, at its page limit or by naming a cursor it
    /// had named before: what it gave until then is kept. Asked for by
    /// name, a server that cannot be listed, or is not connected, fails the
    /// call instead: there is no other server for it to go on with, and a
    /// request for one server is as strict as [`Self::list_resources`].
    pub fn list_resources_with_errors_or_cancel(
        &self,
        server: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpResourceListing, McpRequestError> {
        let clients = match server {
            Some(server) => vec![(
                server.to_string(),
                self.client(server).ok_or_else(|| {
                    McpRequestError::Failed(format!("MCP server '{server}' is not connected"))
                })?,
            )],
            None => self.resource_clients(),
        };

        let named = server.is_some();
        let mut listing = McpResourceListing {
            resources: Vec::new(),
            errors: if named { Vec::new() } else { self.errors() },
        };
        for (server, client) in clients {
            match self
                .listed_resources(&server, &client, should_cancel)
                .map_err(McpRequestError::from_message)
            {
                Ok((listed, warning)) => {
                    listing.resources.extend(listed);
                    listing
                        .errors
                        .extend(warning.map(|warning| format!("{server}: {warning}")));
                }
                Err(error) if named => return Err(error),
                Err(McpRequestError::Cancelled) => return Err(McpRequestError::Cancelled),
                Err(McpRequestError::Failed(error)) => {
                    listing.errors.push(format!("{server}: {error}"));
                }
            }
        }

        Ok(listing)
    }

    /// The resources `server` lists through `client`, read a page at a time,
    /// and a warning when it gave only part of them (see
    /// [`collect_pages`]).
    fn listed_resources(
        &self,
        server: &str,
        client: &Arc<McpClient>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<(Vec<McpResource>, Option<String>), String> {
        let (listed, warning) = collect_pages(|cursor| {
            let result = client.list_resources_or_cancel(cursor, should_cancel);
            self.note_answer(server, client, &result);
            let list: ResourcesListResult =
                serde_json::from_value(result.map_err(|error| error.to_string())?)
                    .map_err(|error| format!("invalid MCP resources/list result: {error}"))?;
            Ok((list.resources, list.next_cursor))
        })?;
        let resources = listed
            .into_iter()
            .map(|resource| McpResource {
                server: server.to_string(),
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime_type: resource.mime_type,
            })
            .collect();
        Ok((resources, warning))
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
            let (listed, warning) = self
                .listed_resource_templates(&server, &client, should_cancel)
                .map_err(McpRequestError::from_message)?;
            // What the server gave of a list it cut short is kept, and noted
            // on the server, as the listing has no errors of its own.
            if let Some(warning) = warning {
                let note = incomplete_list(&server, "resources/templates/list", &warning);
                self.note_server_error(&server, &client, note);
            }
            resource_templates.extend(listed);
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

    /// The resource templates of `server`, or of every server that has
    /// resources, with an error for each server that could not be listed,
    /// and for each list a server cut short, at its page limit or by naming
    /// a cursor it had named before: what it gave until then is kept. Asked
    /// for by name, a server that cannot be listed, or is not connected,
    /// fails the call instead: there is no other server for it to go on
    /// with, and a request for one server is as strict as
    /// [`Self::list_resource_templates`].
    pub fn list_resource_templates_with_errors_or_cancel(
        &self,
        server: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpResourceTemplateListing, McpRequestError> {
        let clients = match server {
            Some(server) => vec![(
                server.to_string(),
                self.client(server).ok_or_else(|| {
                    McpRequestError::Failed(format!("MCP server '{server}' is not connected"))
                })?,
            )],
            None => self.resource_clients(),
        };

        let named = server.is_some();
        let mut listing = McpResourceTemplateListing {
            resource_templates: Vec::new(),
            errors: if named { Vec::new() } else { self.errors() },
        };
        for (server, client) in clients {
            match self
                .listed_resource_templates(&server, &client, should_cancel)
                .map_err(McpRequestError::from_message)
            {
                Ok((listed, warning)) => {
                    listing.resource_templates.extend(listed);
                    listing
                        .errors
                        .extend(warning.map(|warning| format!("{server}: {warning}")));
                }
                Err(error) if named => return Err(error),
                Err(McpRequestError::Cancelled) => return Err(McpRequestError::Cancelled),
                Err(McpRequestError::Failed(error)) => {
                    listing.errors.push(format!("{server}: {error}"));
                }
            }
        }

        Ok(listing)
    }

    /// The resource templates `server` lists through `client`, read a page
    /// at a time, and a warning when it gave only part of them (see
    /// [`collect_pages`]).
    fn listed_resource_templates(
        &self,
        server: &str,
        client: &Arc<McpClient>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<(Vec<McpResourceTemplate>, Option<String>), String> {
        let (listed, warning) = collect_pages(|cursor| {
            let result = client.list_resource_templates_or_cancel(cursor, should_cancel);
            self.note_answer(server, client, &result);
            let list: ResourceTemplatesListResult = serde_json::from_value(
                result.map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("invalid MCP resources/templates/list result: {error}"))?;
            Ok((list.resource_templates, list.next_cursor))
        })?;
        let templates = listed
            .into_iter()
            .map(|template| McpResourceTemplate {
                server: server.to_string(),
                uri_template: template.uri_template,
                name: template.name,
                description: template.description,
                mime_type: template.mime_type,
            })
            .collect();
        Ok((templates, warning))
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
        let result = client.read_resource_or_cancel(uri, should_cancel);
        self.note_answer(server, &client, &result);
        serde_json::from_value(result?).map_err(|error| {
            McpRequestError::Failed(format!("invalid MCP resources/read result: {error}"))
        })
    }
}

/// The number of the connection attempt each server starts with.
const FIRST_CONNECTION: u64 = 1;
/// How often [`McpRegistry::wait_for_startup`] looks again.
const STARTUP_POLL: Duration = Duration::from_millis(25);

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

/// The name Orca gives the server configured as `name`, the one in its
/// tools' names (see [`canonical_mcp_name`]).
pub fn canonical_server_name(name: &str) -> String {
    canonical_mcp_name(name)
}

#[cfg(test)]
mod tests;
